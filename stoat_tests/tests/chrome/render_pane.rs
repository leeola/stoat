use lsp_types::{Diagnostic, DiagnosticSeverity, MessageType, Position, Range};
use ratatui::{buffer::Buffer, layout::Rect};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use stoat::{
    action_handlers::{dispatch, focused_editor_mut},
    agent_ipc::AgentControl,
    app::MessageKind,
    badge::{Anchor, Badge, BadgeSource, BadgeState},
    blame, buffer_lifecycle, commit_picker,
    diff_map::{DiffMap, MarkKind},
    editor_state::EditorState,
    host::{git::FileKind, LspNotification},
    lsp::drain,
    pane::{DockPanel, DockSide, DockVisibility, FocusTarget, View},
    rebase::RebaseState,
    render::{
        conflict_view::ConflictLayout,
        diff_changes,
        pane::{focused_staged_label, muted_tint, name_segments, repo_segments},
        TypeScale,
    },
    term_session::TermId,
    workspace::diff::{RepoStatus, WorktreeBase, DIFF_SETTLE},
    Stoat,
};
use stoat_action::{
    AddSelectionAbove, AddSelectionBelow, Conflict, KeepPrimarySelection, MoveDown, OpenFile,
    OpenSearchInput,
};
use stoat_language::structural_diff;
use tokio::sync::{mpsc, oneshot};

/// The way into a diff screen from an open file, so one test covers both
/// screens.
type EnterScreen = fn(&mut stoat::test_harness::TestHarness);

/// A split diff and a changes view declare the strip a plain editor
/// declares, seven cells in the syntax palette that marks its changes from
/// the class table's first Change Mark class, as a per-pane strip and as
/// the single band alike.
///
/// The strip fills no ground of its own, and its columns end at the text
/// strip's limit. The viewport is the thumb color at the plain strip's
/// alpha, outlined in the same color at the outline's alpha.
#[test]
fn a_diff_screen_declares_a_seven_cell_syntax_strip() {
    let screens: [(&str, EnterScreen); 2] = [
        ("split diff", |h| {
            stoat::action_handlers::review::enter_diff_view(&mut h.stoat);
        }),
        ("changes view", |h| {
            h.cover_with_diff_changes(false);
        }),
    ];
    for mode in [
        stoat_config::MinimapMode::PerPane,
        stoat_config::MinimapMode::Single,
    ] {
        for (screen, enter) in screens {
            let mut h = stoat::test_harness::TestHarness::with_size(120, 12);
            let (tx, mut rx) = mpsc::unbounded_channel();
            h.stoat.set_apc_tx(tx);
            h.stoat.stoatty_protocol = 13;
            h.stoat.settings.editor_minimap = Some(mode);
            h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "A\nb\n")]);
            h.open_file(Path::new("/repo/a.txt"));
            enter(&mut h);
            h.settle();
            for _ in 0..2 {
                h.stoat.render();
                stoat::apc_emit::emit_apc_scene(&mut h.stoat);
            }
            let strips: Vec<_> = stoat::test_fixture::drain_apc(&mut rx)
                .into_iter()
                .filter_map(|command| match command {
                    stoatty_protocol::command::Command::Minimap(strip) => Some((
                        strip.width,
                        strip.max_columns,
                        strip.bg,
                        strip.thumb,
                        strip.thumb_border,
                        strip.palette,
                        strip.mark_class,
                    )),
                    _ => None,
                })
                .collect();

            let theme = &h.stoat.theme;
            let thumb = stoat::render::paint::style_rgb(
                theme.get(stoat::theme::scope::UI_BORDER_FOCUSED).fg,
            )
            .expect("an rgb thumb");
            let declared = (
                stoat::render::editor::MINIMAP_STRIP_COLS,
                stoat::render::pane::MINIMAP_MAX_COLUMNS,
                [0, 0, 0, 0],
                [
                    thumb[0],
                    thumb[1],
                    thumb[2],
                    stoat::render::MINIMAP_THUMB_ALPHA,
                ],
                Some([
                    thumb[0],
                    thumb[1],
                    thumb[2],
                    stoat::render::MINIMAP_THUMB_OUTLINE_ALPHA,
                ]),
                h.stoat.minimap_class_table.palette().to_vec(),
                h.stoat.minimap_class_table.mark_class(),
            );
            assert_eq!(strips.last(), Some(&declared), "{screen} in {mode:?}");
        }
    }
}

/// The view a terminal session shows as, so one test covers each kind of
/// pane a session fills.
type SessionView = fn(TermId) -> View;

/// A file's name reads its directory muted, its stem in the name's style,
/// and a tagged extension in the tag's color, and any other name stays one
/// segment. However the name parts, its texts join to the pad, the name,
/// the modified mark, and one closing space.
#[test]
fn a_status_name_parts_into_directory_stem_and_suffix() {
    let h = Stoat::test();
    let theme = &*h.stoat.theme;
    let row = theme.get(stoat::theme::scope::UI_STATUSBAR_FOCUSED);
    let [name, muted, rust] = [
        stoat::theme::scope::UI_TEXT,
        stoat::theme::scope::UI_TEXT_MUTED,
        stoat::theme::scope::UI_TAG_RUST,
    ]
    .map(|scope| row.fg(theme.get(scope).fg.expect("an ink")));
    let parts = |text: &str, pad: &str, dirty: bool, split: bool| {
        name_segments(text, pad, dirty, split, name, row, theme)
    };
    let seg = |text: &str, style| (text.to_owned(), style);

    assert_eq!(
        [
            parts("src/render/chrome.rs", " ", true, true),
            parts("src/render/chrome.rs", "", false, true),
            parts("Makefile", " ", false, true),
            parts("zsh: ~/src", " ", false, false),
        ],
        [
            vec![
                seg(" src/render/", muted),
                seg("chrome", name),
                seg(".rs", rust),
                seg(" [+] ", muted),
            ],
            vec![
                seg("src/render/", muted),
                seg("chrome", name),
                seg(".rs ", rust),
            ],
            vec![seg(" Makefile ", name)],
            vec![seg(" zsh: ~/src ", name)],
        ],
    );
}

/// The cell fallback inks each split pane's row with the directory muted,
/// the name in the text color on the focused row and dim on the other,
/// the extension in the tag's color, and the position muted.
#[test]
fn each_status_row_inks_the_directory_name_extension_and_position() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 12);
    h.stoat.stoatty = false;
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    let path = PathBuf::from("/repo/src/a.rs");
    h.fake_fs().insert_file(&path, b"fn a() {}\n");
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    h.type_action("SplitRight()");

    let buf = h.render_composited();
    let inks: Vec<_> = pane_status_rects(&h)
        .into_iter()
        .map(|row| {
            let text = status_row(&buf, row);
            let at = |part: &str| row.x + text.find(part).expect("the part shows") as u16;
            [at("src/"), at("a.rs"), at(".rs"), at("1:1")].map(|x| buf[(x, row.y)].fg)
        })
        .collect();

    let theme = &*h.stoat.theme;
    let [text, dim, muted, rust] = [
        stoat::theme::scope::UI_TEXT,
        stoat::theme::scope::UI_TEXT_DIM,
        stoat::theme::scope::UI_TEXT_MUTED,
        stoat::theme::scope::UI_TAG_RUST,
    ]
    .map(|scope| theme.get(scope).fg.expect("an ink"));
    assert_eq!(
        inks,
        [[muted, dim, rust, muted], [muted, text, rust, muted]],
        "the unfocused left row, then the focused right row",
    );
}

/// A blamed pane's status row names `blame` right after its file name, and
/// the other pane's row does not name it.
#[test]
fn a_blamed_pane_names_blame_after_its_file_name() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 12);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    let path = PathBuf::from("/repo/src/a.rs");
    h.fake_fs().insert_file(&path, b"fn a() {}\n");
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    h.type_action("SplitRight()");
    {
        let ws = h.stoat.active_workspace_mut();
        let focus = ws.panes.focus();
        ws.panes.pane_mut(focus).blame = true;
    }

    let buf = h.render_composited();
    let before_blame: Vec<Option<String>> = pane_status_rows(&h, &buf)
        .into_iter()
        .map(|(_, text)| {
            let words: Vec<&str> = text.split_whitespace().collect();
            let at = words.iter().position(|word| *word == "blame")?;
            Some(words[at.checked_sub(1)?].to_string())
        })
        .collect();
    assert_eq!(
        before_blame,
        [None, Some("src/a.rs".to_string())],
        "the left row holds no blame, and the blamed right row holds it after the name",
    );
}

/// A blamed pane's status row names the cursor line's commit left of the
/// position, and names an edited line uncommitted once the typing settles.
#[test]
fn a_blamed_status_row_names_the_commit_of_the_cursor_line() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 12);
    h.seed_linear_history("/repo", &[("c1aaaaaaaa", "one", &[("a.rs", "one\n")])]);
    let now = commit_picker::now_epoch(&h.stoat.executor);
    let time = now - 3 * 86_400;
    h.fake_git()
        .add_repo("/repo")
        .authored("c1aaaaaaaa", "Ada Lovelace", time);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_fs().insert_file("/repo/a.rs", "one\n");
    h.open_file(Path::new("/repo/a.rs"));
    h.type_keys("space h a");
    h.settle();
    let committed = blamed_row_end(&mut h, 32);

    h.edit_focused(0..0, "x");
    h.advance_clock(DIFF_SETTLE + Duration::from_millis(1));
    let commit = format!(
        " c1aaaaa \u{b7} Ada \u{b7} {}  1:1 ",
        blame::short_date(time, 0, now)
    );
    assert_eq!(
        [committed, blamed_row_end(&mut h, 32)],
        [
            format!("{commit:>32}"),
            format!("{:>32}", " uncommitted  1:2 ")
        ],
    );
}

/// The last `width` cells of the focused pane's status row, painted.
fn blamed_row_end(h: &mut stoat::test_harness::TestHarness, width: usize) -> String {
    let buf = h.render_composited();
    let (_, text) = pane_status_rows(h, &buf).remove(0);
    let cells: Vec<char> = text.chars().collect();
    cells[cells.len() - width..].iter().collect()
}

/// The bar paints a message in the color of its kind, on a pane and on an
/// overlay screen.
#[test]
fn the_status_bar_paints_a_message_in_the_color_of_its_kind() {
    use ratatui::style::Color;

    let (config, _) = stoat_config::parse(
        r##"theme t { ui.message.error.fg = "#ff0000"; ui.message.info.fg = "#00ff00"; }"##,
    );
    let theme = Arc::new(
        stoat::theme::Theme::from_config(&config.expect("the theme parses"), "t")
            .expect("the theme loads"),
    );
    let inks = |overlay: bool| {
        let mut h = stoat::test_harness::TestHarness::with_size(60, 12);
        h.stoat.stoatty = false;
        h.stoat.theme = theme.clone();
        if overlay {
            h.stoat.active_workspace_mut().rebase = Some(RebaseState::new(
                PathBuf::from("/overlay"),
                "onto".into(),
                vec![],
            ));
        }
        [MessageKind::Alert, MessageKind::Info].map(|kind| {
            h.stoat.notify(kind, "zq");
            ink_of(&h.render_composited(), "zq")
        })
    };

    let [red, green] = [Color::Rgb(0xff, 0, 0), Color::Rgb(0, 0xff, 0)].map(Some);
    assert_eq!([inks(false), inks(true)], [[red, green], [red, green]]);
}

/// The foreground of the first cell of `text` in `buf`, read row by row.
fn ink_of(buf: &Buffer, text: &str) -> Option<ratatui::style::Color> {
    (0..buf.area.height).find_map(|y| {
        let row: String = (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .replace('─', " ");
        let at = row[..row.find(text)?].chars().count() as u16;
        Some(buf[(at, y)].fg)
    })
}

/// default_dark's focused status row inks its directory and its position
/// in the lifted muted color, and its unfocused row in the theme's muted
/// color.
#[test]
fn default_dark_s_focused_row_shows_its_muted_segments() {
    use ratatui::style::Color;

    let (config, _) = stoat_config::parse(stoat::app::DEFAULT_KEYMAP);
    let config = config.expect("the shipped config parses");
    let mut h = stoat::test_harness::TestHarness::with_size(120, 12);
    h.stoat.stoatty = false;
    h.stoat.theme = Arc::new(
        stoat::theme::Theme::from_config(&config, "default_dark").expect("the theme loads"),
    );
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    let path = PathBuf::from("/repo/src/a.rs");
    h.fake_fs().insert_file(&path, b"fn a() {}\n");
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    h.type_action("SplitRight()");

    let buf = h.render_composited();
    let inks: Vec<_> = pane_status_rects(&h)
        .into_iter()
        .map(|row| {
            let text = status_row(&buf, row);
            let at = |part: &str| row.x + text.find(part).expect("the part shows") as u16;
            [at("src/"), at("1:1")].map(|x| buf[(x, row.y)].fg)
        })
        .collect();

    let [muted, lifted] =
        [[0x63, 0x6d, 0x83], [0xb4, 0xb9, 0xc4]].map(|[r, g, b]| Color::Rgb(r, g, b));
    assert_eq!(
        inks,
        [[muted, muted], [lifted, lifted]],
        "the unfocused left row, then the focused right row",
    );
}

fn diag(severity: DiagnosticSeverity) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        },
        severity: Some(severity),
        message: String::new(),
        ..Default::default()
    }
}

fn open_rust(h: &mut stoat::test_harness::TestHarness) {
    let root = PathBuf::from("/lsp");
    let path = root.join("a.rs");
    h.fake_fs().insert_file(&path, b"fn main() {}");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
}

/// The symbols of `buf`'s bottom row, the status row. A split diff's
/// divider cell reads U+2502 there, the glyph the composite draws for the
/// bars that fill the cell.
fn bar_row(buf: &Buffer) -> String {
    let y = buf.area.height - 1;
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

/// A buffer outside the workspace falls back to `~`-abbreviating against the
/// home the frame carries, which comes from the env host so a test controls
/// it. Resolving it in the paint instead would read the real environment.
#[test]
fn a_pane_status_abbreviates_against_the_frames_home() {
    let paint = |home: Option<&str>| {
        let mut h = stoat::test_harness::TestHarness::with_size(60, 8);
        if let Some(home) = home {
            h.fake_env().set("HOME", home);
            // The cached home resolves when the env host is set, so re-inject
            // it now that HOME is populated.
            h.stoat.set_env_host(h.fake_env().clone());
        }

        let path = PathBuf::from("/fixture-home/notes.txt");
        h.fake_fs().insert_file(&path, b"hello");
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/elsewhere");
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        bar_row(&h.render_composited())
    };

    assert!(
        paint(Some("/fixture-home")).contains("~/notes.txt"),
        "a path under the frame's home paints abbreviated, got {:?}",
        paint(Some("/fixture-home")),
    );
    assert!(
        paint(None).contains("/fixture-home/notes.txt"),
        "and stays whole with no home to measure against, got {:?}",
        paint(None),
    );
}

/// A shell or an agent in a pane names itself through its title, and the
/// bar keeps the kind label until the child sets one.
#[test]
fn a_terminal_or_agent_pane_status_names_the_childs_title() {
    let kinds: [(SessionView, &str); 2] = [(View::Terminal, "[term]"), (View::Agent, "[agent]")];
    let bars: Vec<(bool, bool, bool)> = kinds
        .iter()
        .map(|&(view, label)| {
            let (before, after) = bar_before_and_after_a_title(view, None);
            (
                before.contains(label),
                after.contains("build"),
                after.contains(label),
            )
        })
        .collect();
    assert_eq!(bars, [(true, true, false); 2]);
}

/// A session that a handoff started names itself by its label until the
/// child sets a title, in a terminal pane and in an agent pane alike.
#[test]
fn a_handoff_session_s_pane_status_names_its_label() {
    let views: [SessionView; 2] = [View::Terminal, View::Agent];
    let bars = views.map(|view| {
        let (before, after) = bar_before_and_after_a_title(view, Some("mytui"));
        (
            before.contains("mytui"),
            after.contains("build"),
            after.contains("mytui"),
        )
    });
    assert_eq!(bars, [(true, true, false); 2]);
}

/// Show a fresh terminal session in the focused pane as `view`, with the
/// handoff label `label`, and paint its bar before and after the child
/// titles it `build`.
fn bar_before_and_after_a_title(view: SessionView, label: Option<&str>) -> (String, String) {
    let mut h = stoat::test_harness::TestHarness::with_size(60, 8);
    dispatch(&mut h.stoat, &stoat_action::Terminal);
    let term_id = {
        let ws = h.stoat.active_workspace_mut();
        let focus = ws.panes.focus();
        let View::Terminal(term_id) = ws.panes.pane(focus).view else {
            panic!("the terminal action shows a terminal");
        };
        ws.panes.pane_mut(focus).view = view(term_id);
        ws.terms[term_id].label = label.map(str::to_owned);
        term_id
    };
    let before = bar_row(&h.render_composited());
    h.stoat.active_workspace_mut().terms[term_id]
        .term
        .feed(b"\x1b]0;build\x07");
    (before, bar_row(&h.render_composited()))
}

/// Push a work-done progress begin so `fake`'s server reads as busy, painting
/// its bar badge.
fn mark_busy(h: &mut stoat::test_harness::TestHarness, fake: &stoat::host::FakeLsp) {
    use lsp_types::{NumberOrString, WorkDoneProgress, WorkDoneProgressBegin};
    use stoat::host::LspNotification;

    fake.push_notification(LspNotification::Progress {
        token: NumberOrString::Number(1),
        value: WorkDoneProgress::Begin(WorkDoneProgressBegin {
            title: "indexing".into(),
            cancellable: None,
            message: None,
            percentage: None,
        }),
    });
    h.drain_lsp();
}

/// After the servers, the bar names each source the file's diagnostics come
/// from, once, in the order they first appear.
#[test]
fn the_status_row_names_each_diagnostic_source_once() {
    let mut h = Stoat::test();
    h.install_lsp_server("rust", "rust-analyzer");
    open_rust(&mut h);
    let sourced = |source: &str| Diagnostic {
        source: Some(source.to_string()),
        ..diag(DiagnosticSeverity::WARNING)
    };
    h.seed_diagnostics(
        "/lsp/a.rs",
        vec![sourced("rustc"), sourced("clippy"), sourced("rustc")],
    );

    let bar = bar_row(&h.render_composited());
    assert!(
        bar.contains(" rust-analyzer \u{b7} rustc \u{b7} clippy "),
        "the server, then each source once:\n{bar}"
    );
}

/// An attached server shows its name while idle, with no spinner, and
/// stamps the hover rect that opens its status.
#[test]
fn lsp_badge_names_an_idle_server() {
    let mut h = Stoat::test();
    h.install_lsp_server("rust", "rust-analyzer");
    open_rust(&mut h);

    let buf = h.render_composited();
    let bar = bar_row(&buf);
    assert_eq!(
        (
            bar.contains(" rust-analyzer "),
            bar.contains('⠋'),
            h.stoat.lsp_badge_rect.is_some(),
        ),
        (true, false, true),
        "the name, no spinner, and the hover rect:\n{bar}"
    );
}

#[test]
fn lsp_badge_shows_spinner_when_busy() {
    let mut h = Stoat::test();
    let fake = h.install_lsp_server("rust", "rust-analyzer");
    open_rust(&mut h);
    mark_busy(&mut h, &fake);

    let buf = h.render_composited();
    let bar = bar_row(&buf);
    assert!(
        bar.contains("⠋ rust-analyzer "),
        "a busy server shows the spinner before its name:\n{bar}"
    );
}

#[test]
fn diff_warm_shows_spinner_segment_while_busy() {
    let mut h = stoat::test_harness::TestHarness::with_size(100, 12);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "b\n")]);
    h.stoat.set_diff_warm_auto(true);
    // The warm is opt-in, so a spinner for it needs the option on.
    h.stoat.settings.review_precompute = Some(true);
    stoat::diff_warm::ensure_diff_warm(&mut h.stoat);

    let buf = h.render_composited();
    let bar = bar_row(&buf);
    assert!(
        bar.contains("diff"),
        "a pending warm shows the diff segment:\n{bar}"
    );
    assert!(
        bar.contains('⠋'),
        "the diff segment shows the spinner glyph:\n{bar}"
    );

    h.settle();
    stoat::diff_warm::install_finished(&mut h.stoat);
    let buf = h.render_composited();
    let bar = bar_row(&buf);
    assert!(
        !bar.contains("diff"),
        "the segment clears once the warm finishes:\n{bar}"
    );
}

/// The server list is held across frames, so installing a server has to
/// invalidate it. Without that the bar would keep painting the list it
/// derived before the server existed.
#[test]
fn a_server_installed_after_a_paint_reaches_the_bar() {
    let mut h = Stoat::test();
    open_rust(&mut h);

    let bar = bar_row(&h.render_composited());
    assert!(
        !bar.contains("rust-analyzer"),
        "no server is up yet:\n{bar}"
    );

    let fake = h.install_lsp_server("rust", "rust-analyzer");
    mark_busy(&mut h, &fake);
    let bar = bar_row(&h.render_composited());
    assert!(
        bar.contains("rust-analyzer"),
        "the newly installed server badges on the next paint:\n{bar}"
    );
}

/// The names are held across frames but the busy flags are not, since only
/// they can move without the focus or the registry moving.
#[test]
fn a_server_going_busy_after_a_paint_shows_its_spinner() {
    let mut h = Stoat::test();
    let fake = h.install_lsp_server("rust", "rust-analyzer");
    open_rust(&mut h);

    let bar = bar_row(&h.render_composited());
    assert!(
        bar.contains(" rust-analyzer ") && !bar.contains('⠋'),
        "an idle server paints its name with no spinner:\n{bar}"
    );

    mark_busy(&mut h, &fake);
    let bar = bar_row(&h.render_composited());
    assert!(
        bar.contains("⠋ rust-analyzer "),
        "the flag refreshes without the names being rebuilt:\n{bar}"
    );
}

#[test]
fn lsp_badge_one_per_server() {
    let mut h = Stoat::test();
    let ra = h.install_lsp_server("rust", "rust-analyzer");
    let ss = h.install_lsp_server("rust", "second-server");
    open_rust(&mut h);
    mark_busy(&mut h, &ra);
    mark_busy(&mut h, &ss);

    let buf = h.render_composited();
    let bar = bar_row(&buf);
    assert!(
        bar.contains("rust-analyzer") && bar.contains("second-server"),
        "each running server gets its own badge:\n{bar}"
    );
}

/// The badge's hover rect comes from the status row, which a replayed
/// focused pane still builds, so a hover over the badge still finds it.
#[test]
fn a_replayed_focused_pane_keeps_the_lsp_badge_rect() {
    let mut h = Stoat::test();
    let fake = h.install_lsp_server("rust", "rust-analyzer");
    open_rust(&mut h);
    mark_busy(&mut h, &fake);
    // The first paint installs the wrap width, which moves the paint
    // version past the key it captured. The second captures the settled
    // key.
    let _ = h.stoat.render();
    let _ = h.stoat.render();
    let badge = h.stoat.lsp_badge_rect;
    assert!(badge.is_some(), "the busy server raises the badge");

    let painted_before = h.stoat.pane_paints;
    let _ = h.stoat.render();
    assert_eq!(
        (h.stoat.pane_paints - painted_before, h.stoat.lsp_badge_rect),
        (0, badge),
        "the replayed pane hands back the badge rect"
    );
}

#[test]
fn lsp_badge_hidden_on_unfocused_pane() {
    let mut h = Stoat::test();
    let fake = h.install_lsp_server("rust", "rust-analyzer");
    open_rust(&mut h);
    mark_busy(&mut h, &fake);
    h.type_action("SplitRight()");
    h.settle();

    let buf = h.render_composited();
    let rendered: String = (buf.area.y..buf.area.y + buf.area.height)
        .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
        .map(|(x, y)| buf[(x, y)].symbol().to_string())
        .collect();
    assert_eq!(
        rendered.matches("rust-analyzer").count(),
        1,
        "only the focused pane paints a badge:\n{rendered}"
    );
}

#[test]
fn lsp_badge_rect_records_painted_columns() {
    let mut h = Stoat::test();
    let fake = h.install_lsp_server("rust", "rust-analyzer");
    open_rust(&mut h);
    mark_busy(&mut h, &fake);

    let buf = h.render_composited();
    let rect = h.stoat.lsp_badge_rect.expect("badge rect stamped");
    let span: String = (rect.x..rect.x + rect.width)
        .map(|x| buf[(x, rect.y)].symbol())
        .collect();
    // Under cfg(test) every type role is a full cell, so the busy badge
    // (" ⠋ rust-analyzer ", 17 chars) covers 16 cells. The leftmost pad
    // space carries no glyph and is dropped.
    assert_eq!(
        rect.width, 16,
        "the rect is the scaled extent of the drawn glyphs, not their full char span"
    );
    assert!(
        span.contains("rust-analyzer"),
        "and covers the drawn badge glyph columns:\n{span}"
    );
}

fn full_render(h: &mut stoat::test_harness::TestHarness) -> String {
    let buf = h.render_composited();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn toggle_lsp_status_shows_and_hides_the_card() {
    use lsp_types::{NumberOrString, WorkDoneProgress, WorkDoneProgressBegin};
    use stoat::host::LspNotification;

    let mut h = Stoat::test();
    let fake = h.install_lsp_server("rust", "rust-analyzer");
    open_rust(&mut h);
    fake.push_notification(LspNotification::Progress {
        token: NumberOrString::Number(1),
        value: WorkDoneProgress::Begin(WorkDoneProgressBegin {
            title: "indexing".into(),
            cancellable: None,
            message: None,
            percentage: None,
        }),
    });
    h.drain_lsp();

    assert!(
        !full_render(&mut h).contains("indexing"),
        "the card stays hidden until toggled open"
    );

    dispatch(&mut h.stoat, &stoat_action::ToggleLspStatus);
    assert!(
        full_render(&mut h).contains("indexing"),
        "toggling pins the card with the in-flight entry row"
    );

    dispatch(&mut h.stoat, &stoat_action::ToggleLspStatus);
    assert!(
        !full_render(&mut h).contains("indexing"),
        "toggling again hides the card"
    );
}

#[test]
fn pinned_lsp_status_lists_idle_servers() {
    let mut h = Stoat::test();
    h.install_lsp_server("rust", "rust-analyzer");
    open_rust(&mut h);
    dispatch(&mut h.stoat, &stoat_action::ToggleLspStatus);

    assert!(
        full_render(&mut h).contains("RA idle"),
        "pinned with no in-flight work lists each running server as idle"
    );
}

#[test]
fn error_card_stacks_above_the_pinned_status_card() {
    use lsp_types::MessageType;

    let mut h = Stoat::test();
    h.install_lsp_server("rust", "rust-analyzer");
    open_rust(&mut h);
    dispatch(&mut h.stoat, &stoat_action::ToggleLspStatus);
    h.stoat.lsp_message = Some((MessageType::ERROR, "workspace load failed".to_string()));

    let buf = h.render_composited();
    let rows: Vec<String> = (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
        .collect();
    let status_y = rows
        .iter()
        .position(|r| r.contains("RA idle"))
        .expect("pinned status card painted");
    let error_y = rows
        .iter()
        .position(|r| r.contains("workspace"))
        .expect("error card painted");
    assert!(
        error_y < status_y,
        "the error card auto-shows and stacks above the pinned status card"
    );
}

/// A server error of several lines paints as one line of the error card.
/// A plain terminal takes the card text cell by cell, and flushing a frame
/// with a raw newline in a cell trips ratatui's control-character check.
#[test]
fn a_multi_line_server_error_paints_as_one_line() {
    let mut h = Stoat::test();
    h.stoat.stoatty = false;
    let fake = h.install_lsp_server("rust", "rust-analyzer");
    open_rust(&mut h);
    fake.push_notification(LspNotification::ShowMessage {
        typ: MessageType::ERROR,
        message: "failed to fetch workspace\n\nno Cargo.toml".to_string(),
    });
    drain::drain_lsp_notifications(&mut h.stoat);

    h.snapshot();
    let frame = h.rendered_buffer();
    // The same cell measure a terminal frontend runs when it flushes.
    Buffer::empty(frame.area).diff(frame);
    assert!(
        h.rendered_text()
            .contains("failed to fetch workspace no Cargo.toml"),
        "the error card joins the lines with one space",
    );
}

#[test]
fn badge_hover_opens_the_status_card() {
    let mut h = Stoat::test();
    h.install_lsp_server("rust", "rust-analyzer");
    open_rust(&mut h);

    assert!(
        !full_render(&mut h).contains("RA idle"),
        "no card without hover or pin"
    );

    h.stoat.lsp_badge_hovered = true;
    assert!(
        full_render(&mut h).contains("RA idle"),
        "hovering the badge opens the idle card"
    );

    h.stoat.lsp_badge_hovered = false;
    assert!(
        !full_render(&mut h).contains("RA idle"),
        "un-hovering closes the card"
    );

    h.stoat.lsp_status_pinned = true;
    assert!(
        full_render(&mut h).contains("RA idle"),
        "pinning keeps the card open regardless of hover"
    );
}

#[test]
fn snapshot_status_bar_diagnostic_badge_warning_color() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/diag-status");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    h.seed_diagnostics(path, vec![diag(DiagnosticSeverity::WARNING)]);
    h.assert_snapshot("status_bar_diagnostic_badge_warning_color");
}

/// The pencil count takes `ui.statusbar.unstaged` and the plus count
/// `ui.statusbar.staged`, which the shipped theme reads as its accent and
/// teal.
#[test]
fn the_staged_label_colors_each_count() {
    use ratatui::style::{Color, Style};

    let h = Stoat::test();
    let segments = focused_staged_label(
        MarkKind::Staged,
        None,
        None,
        Some((1, 4)),
        None,
        (Style::default(), &*h.stoat.theme),
    )
    .expect("a label");
    let colors: Vec<(String, Option<Color>)> = segments
        .into_iter()
        .map(|(text, style)| (text, style.fg))
        .collect();
    assert_eq!(
        colors,
        [
            (" \u{270e}4".to_string(), Some(Color::Rgb(0x7f, 0xb0, 0xff))),
            (" \u{271a}1".to_string(), Some(Color::Rgb(0x6c, 0xc7, 0xd6))),
            (" ".to_string(), None),
        ],
    );
}

/// Each piece of the repo segment reads in its own color. The glyph is dim,
/// the head bold, the divergence muted, the rebase chip on the rebase
/// color, the divider in the overlay's line color, and each count in its
/// scope.
#[test]
fn the_repo_segment_colors_each_piece() {
    use ratatui::style::{Color, Modifier, Style};

    let h = Stoat::test();
    let theme = &*h.stoat.theme;
    let repo = RepoStatus {
        head: Some("main".to_string()),
        staged: 1,
        unstaged: 2,
        untracked: 3,
        ahead: 2,
        behind: 1,
        rebasing: true,
        merge_head: None,
    };
    let fg = |scope: &str| theme.get(scope).fg;
    let segments: Vec<(String, Option<Color>, Option<Color>, bool)> =
        repo_segments(&repo, Style::default(), theme)
            .into_iter()
            .map(|(text, style)| {
                let bold = style.add_modifier.contains(Modifier::BOLD);
                (text, style.fg, style.bg, bold)
            })
            .collect();
    let rebase = fg(stoat::theme::scope::UI_STATUSLINE_REBASE);
    let chip = fg(stoat::theme::scope::UI_MODE_LABEL);
    assert_eq!(
        segments,
        [
            (
                " \u{2387} ".to_string(),
                fg(stoat::theme::scope::UI_TEXT_DIM),
                None,
                false
            ),
            (
                "main".to_string(),
                fg(stoat::theme::scope::UI_TEXT),
                None,
                true
            ),
            (
                " \u{2191}2 \u{2193}1".to_string(),
                fg(stoat::theme::scope::UI_TEXT_MUTED),
                None,
                false
            ),
            (" ".to_string(), None, None, false),
            (" REB ".to_string(), chip, rebase, true),
            (
                " \u{2502}".to_string(),
                fg(stoat::theme::scope::UI_SURFACE_OVERLAY),
                None,
                false
            ),
            (
                " \u{271a} 1 staged".to_string(),
                fg(stoat::theme::scope::DIFF_STAGED),
                None,
                false
            ),
            (
                " \u{270e} 2 unstaged".to_string(),
                fg(stoat::theme::scope::UI_TEXT_MUTED),
                None,
                false
            ),
            (
                " ? 3 untracked".to_string(),
                fg(stoat::theme::scope::UI_TEXT_DIM),
                None,
                false
            ),
        ],
    );
}

#[test]
fn a_clean_repo_reads_in_the_clean_color() {
    use ratatui::style::Style;

    let h = Stoat::test();
    let theme = &*h.stoat.theme;
    let clean = RepoStatus {
        head: None,
        staged: 0,
        unstaged: 0,
        untracked: 0,
        ahead: 0,
        behind: 0,
        rebasing: false,
        merge_head: None,
    };
    let segments: Vec<_> = repo_segments(&clean, Style::default(), theme)
        .into_iter()
        .map(|(text, style)| (text, style.fg))
        .collect();
    assert_eq!(
        segments,
        [(
            " \u{2713} clean".to_string(),
            theme.get(stoat::theme::scope::UI_STATUSBAR_CLEAN).fg
        )],
    );
}

/// A pair names its base file in its own pane's status row alone, and a
/// git diff in another tab names the workspace base there. Neither row
/// repeats the base as a staged lead, since its metrics name it.
#[test]
fn each_diff_s_status_row_names_its_own_base() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 20);
    h.stage_review_scenario("/repo", &[("c.txt", "x\ny\nz\n", "x\ny\nZ\n")]);
    h.open_side_by_side(("/repo/a.txt", "one\ntwo\n"), ("/repo/b.txt", "one\nTWO\n"));
    dispatch(&mut h.stoat, &stoat_action::DiffPair);
    h.settle();
    dispatch(&mut h.stoat, &stoat_action::NewTab);
    h.open_file(Path::new("/repo/c.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    h.settle();

    let named = |h: &mut stoat::test_harness::TestHarness| {
        h.snapshot();
        let text = h.rendered_text().replace('─', " ");
        [
            text.contains("vs HEAD"),
            text.contains("vs a.txt"),
            text.contains("diff vs a.txt"),
        ]
    };
    let git_tab = named(&mut h);
    dispatch(&mut h.stoat, &stoat_action::PrevTab);
    h.settle();
    assert_eq!(
        [git_tab, named(&mut h)],
        [[true, false, false], [false, true, false]],
        "each tab's diff names its own base",
    );
}

/// A per-file diff of a file that the tally names moved, untracked, new, or
/// deleted reads its kind in a File Kind Tag between the lead and the tail
/// of its metrics. The tag draws as one box in its background over the
/// tag's own cells, 1 px inside the row's top and bottom and 2 px inside
/// each end. A unified diff's inline row reads the same after the file's
/// name.
#[test]
fn a_diff_s_status_row_tags_its_file_s_kind() {
    use stoat::{render::paint::style_rgb, test_harness::TestHarness};
    use stoatty_protocol::command::{decode_stream, Command, Edge};

    // The row from the lead on, as long as the metrics, and the row's text
    // under each panel in the tag's fill, with the panel's edges.
    let read = |mut h: TestHarness, path: &str, base: WorktreeBase, kind: FileKind, text: &str| {
        h.stoat.set_diff_warm_auto(true);
        h.stoat.active_workspace_mut().set_worktree_base(base);
        h.open_file(Path::new(path));
        dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
        h.settle_diff_jobs();
        h.type_keys("] g");
        let row: Vec<char> = bar_row(&h.render_composited()).chars().collect();
        let fill =
            style_rgb(diff_changes::kind_tag_style(&h.stoat.theme, &kind).bg).expect("an RGB tag");
        let tags: Vec<(String, [Edge; 4])> = decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .filter_map(|command| match command {
                Command::Panel(panel) if panel.fill == Some(fill) => {
                    let cells = usize::from(panel.left)..usize::from(panel.left + panel.width);
                    Some((row[cells].iter().collect(), panel.edges))
                },
                _ => None,
            })
            .collect();
        let row: String = row.into_iter().collect();
        let lead = text.split('\u{b7}').next().unwrap_or_default();
        let shown = row.find(lead).map(|at| {
            row[at..]
                .chars()
                .take(text.chars().count())
                .collect::<String>()
        });
        (shown, tags)
    };
    let repo = |h: &mut TestHarness| {
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
    };
    let moved = |width| {
        let mut h = TestHarness::with_size(width, 12);
        h.stage_rename_scenario("/repo", "old.rs", "new.rs", "one\ntwo\n", "one\nTWO\n");
        h.fake_git().add_repo("/repo").similarity("new.rs", 94);
        h
    };
    let untracked = {
        let mut h = TestHarness::with_size(120, 12);
        repo(&mut h);
        h.fake_git().add_repo("/repo").untracked("u.rs");
        h.fake_fs().insert_file(Path::new("/repo/u.rs"), b"fresh\n");
        h
    };
    let new = || {
        let mut h = TestHarness::with_size(120, 12);
        repo(&mut h);
        h.fake_git()
            .add_repo("/repo")
            .with_fs(h.fake_fs())
            .index_file("n.rs", "a\nb\n")
            .unstaged_file("n.rs", "a\nb\nc\n");
        h
    };
    let deleted = {
        let mut h = TestHarness::with_size(120, 12);
        repo(&mut h);
        h.fake_git().add_repo("/repo").deleted("d.rs", "gone\n");
        h
    };

    let moved_kind = FileKind::Moved {
        from: PathBuf::from("old.rs"),
        similarity: 94,
    };
    let (moved_text, moved_tag) = (
        " vs HEAD \u{b7}  moved \u{b7} 94%  from old.rs \u{b7} hunk 1 / 1 ",
        " moved \u{b7} 94% ",
    );
    let cases = [
        (
            moved(120),
            "/repo/new.rs",
            WorktreeBase::Head,
            moved_kind.clone(),
            moved_text,
            moved_tag,
        ),
        (
            moved(80),
            "/repo/new.rs",
            WorktreeBase::Head,
            moved_kind,
            moved_text,
            moved_tag,
        ),
        (
            untracked,
            "/repo/u.rs",
            WorktreeBase::Index,
            FileKind::Untracked,
            " no base \u{b7}  untracked  not in the index ",
            " untracked ",
        ),
        (
            new(),
            "/repo/n.rs",
            WorktreeBase::Index,
            FileKind::New,
            " vs index \u{b7}  new  staged 2 / 3 \u{b7} hunk 1 / 1 ",
            " new ",
        ),
        (
            new(),
            "/repo/n.rs",
            WorktreeBase::Head,
            FileKind::New,
            " vs HEAD \u{b7}  new  no base ",
            " new ",
        ),
        (
            deleted,
            "/repo/d.rs",
            WorktreeBase::Head,
            FileKind::Deleted,
            " vs HEAD \u{b7}  deleted  hunk 1 / 1 ",
            " deleted ",
        ),
    ];
    let inset = [
        Edge::Inset(1),
        Edge::Inset(2),
        Edge::Inset(1),
        Edge::Inset(2),
    ];
    let (shown, wanted): (Vec<_>, Vec<_>) = cases
        .into_iter()
        .map(|(h, path, base, kind, text, tag)| {
            (
                read(h, path, base, kind, text),
                (Some(text.to_string()), vec![(tag.to_string(), inset)]),
            )
        })
        .unzip();
    assert_eq!(shown, wanted);
}

/// A split diff view's status row reads the diff's metrics in its base
/// half, with the counts against the divider. Its target half is the
/// pane's own row, with the mode chip, the file name, and the position.
#[test]
fn diff_view_status_halves() {
    let mut h = status_halves_harness();
    h.type_keys("j j j");
    h.assert_snapshot("diff_view_status_halves");
}

/// From the nudge protocol on, each status row is a band. It takes two rows
/// under its pane, its island reaches into the first, and its lane nudges
/// up by the pad and back. A stoatty before the nudge keeps one row, a
/// flush island bottom, and no nudge.
///
/// Two stacked panes at 160 by 50 with no tab bar keep their status rows
/// on rows 24 and 49 either way. The bands take one row from each pane.
/// The single minimap band's island, the third, ends as the lower pane's
/// island does.
#[test]
fn a_status_row_is_a_band_from_the_nudge_protocol_on() {
    use stoatty_protocol::{
        command::{decode_stream, Command, Edge},
        NUDGE_PROTOCOL,
    };

    let layout = |protocol: u32| {
        let mut h = stoat::test_harness::TestHarness::with_size(160, 50);
        h.stoat.stoatty_protocol = protocol;
        h.type_action("SplitDown()");
        h.settle();
        h.stoat.render();

        let mut areas: Vec<Rect> = h
            .stoat
            .active_workspace()
            .panes
            .split_panes()
            .map(|(_, pane)| pane.area)
            .collect();
        areas.sort_by_key(|area| area.y);
        let rows: Vec<u16> = areas
            .iter()
            .filter_map(|&area| {
                stoat::render::pane_status_row(
                    area,
                    h.stoat.single_minimap_rect,
                    h.stoat.size(),
                    h.stoat.status_rows(),
                )
            })
            .map(|row| row.y)
            .collect();
        let lane = decode_stream(h.stoat.apc_scene.lane_bytes(stoat::render::STATUS_LANE));
        let nudges: Vec<Command> = [lane.first(), lane.last()]
            .into_iter()
            .flatten()
            .filter(|command| matches!(command, Command::Nudge(_)))
            .cloned()
            .collect();
        let bottoms: Vec<Edge> = h
            .stoat
            .apc_scene
            .panels()
            .filter(|panel| panel.backdrop)
            .map(|panel| panel.edges[2])
            .collect();
        let spans: Vec<(u16, u16)> = areas.iter().map(|area| (area.y, area.height)).collect();
        (spans, rows, nudges, bottoms)
    };

    assert_eq!(
        [layout(NUDGE_PROTOCOL), layout(NUDGE_PROTOCOL - 1)],
        [
            (
                vec![(0, 23), (25, 23)],
                vec![24, 49],
                vec![Command::Nudge([0, -3]), Command::Nudge([0, 0])],
                vec![Edge::Margin(6); 3],
            ),
            (
                vec![(0, 24), (25, 24)],
                vec![24, 49],
                vec![],
                vec![Edge::Inset(0); 3],
            ),
        ],
        "the pane rows, the status rows, the lane's first and last nudge, and the island \
         bottoms",
    );
}

/// From the nudge protocol on, the tab bar is a band over the top two
/// rows. Lane 0 nudges the bar down by the pad before its text runs and
/// back after them, the active tab's pill stands out of its row, and the
/// islands of the top pane and the single minimap band reach up into the
/// band. A stoatty before the nudge keeps the one-row bar, the chip's box,
/// and flush island tops. A hidden bar takes no rows, and no island reaches
/// above its pane.
///
/// Two stacked panes at 160 by 50 under a bar that always shows start on
/// rows 2 and 26 with the band, and on rows 1 and 25 without it. With the
/// bar hidden, the band's protocol starts them on rows 0 and 25.
#[test]
fn the_tab_bar_is_a_band_from_the_nudge_protocol_on() {
    use stoat_config::TabBarMode;
    use stoatty_protocol::{
        command::{decode_stream, Command, Edge},
        NUDGE_PROTOCOL,
    };

    let layout = |protocol: u32, bar: TabBarMode| {
        let mut h = stoat::test_harness::TestHarness::with_size(160, 50);
        h.stoat.stoatty_protocol = protocol;
        h.stoat.settings.ui_tab_bar = Some(bar);
        h.type_action("SplitDown()");
        h.settle();
        h.stoat.render();

        let mut tops: Vec<u16> = h
            .stoat
            .active_workspace()
            .panes
            .split_panes()
            .map(|(_, pane)| pane.area.y)
            .collect();
        tops.sort_unstable();

        let lane = decode_stream(h.stoat.apc_scene.lane_bytes(0));
        let mut order: Vec<Option<[i16; 2]>> = lane
            .iter()
            .filter_map(|command| match command {
                Command::Nudge(offset) => Some(Some(*offset)),
                Command::TextRun(_) | Command::TextRuns(_) => Some(None),
                _ => None,
            })
            .collect();
        order.dedup();
        let pills: Vec<[Edge; 4]> = lane
            .iter()
            .filter_map(|command| match command {
                Command::Panel(panel) => Some(panel.edges),
                _ => None,
            })
            .collect();

        let mut islands: Vec<(u16, Edge)> = h
            .stoat
            .apc_scene
            .panels()
            .filter(|panel| panel.backdrop)
            .map(|panel| (panel.top, panel.edges[0]))
            .collect();
        islands.sort_unstable_by_key(|&(top, _)| top);
        let island_tops: Vec<Edge> = islands.into_iter().map(|(_, edge)| edge).collect();

        (tops, order, pills, island_tops)
    };

    let pill = Edge::Outset(2);
    assert_eq!(
        [
            layout(NUDGE_PROTOCOL, TabBarMode::Always),
            layout(NUDGE_PROTOCOL - 1, TabBarMode::Always),
            layout(NUDGE_PROTOCOL, TabBarMode::Never),
        ],
        [
            (
                vec![2, 26],
                vec![Some([0, 7]), None, Some([0, 0])],
                vec![[pill, Edge::Inset(0), pill, Edge::Inset(0)]],
                vec![Edge::Margin(14), Edge::Margin(14), Edge::Inset(0)],
            ),
            (
                vec![1, 25],
                vec![None],
                vec![Edge::symmetric(0, 1)],
                vec![Edge::Inset(0); 3],
            ),
            (vec![0, 25], vec![], vec![], vec![Edge::Inset(0); 3]),
        ],
        "the pane tops, lane 0's nudges around its text runs, the pill's edges, and the \
         island tops",
    );
}

/// A 120 by 8 split diff view of one changed `a.rs`, with no minimap and
/// its hints box hidden.
fn status_halves_harness() -> stoat::test_harness::TestHarness {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 8);
    h.stoat.minimap_override = Some(false);
    // The diff view keeps its hints box up, and 8 rows leave it over the
    // whole pane above the status row.
    h.stoat.screen_hints_hidden = true;
    h.stage_review_scenario("/repo", &[("a.rs", "a\nb\nc\nold\n", "a\nb\nc\nnew\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.rs"));
    h.settle_diff_jobs();
    focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .set_diff_view(true);
    h
}

#[test]
fn statusline_shows_staged_and_unstaged_counts() {
    let mut h = stoat::test_harness::TestHarness::with_size(100, 12);
    h.stage_index_scenario(
        "/repo",
        &[("f.txt", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n")],
    );
    // The repo pair counts files off git status, where the file pair counts
    // hunks off the diff map, so the fixture registers a second file to
    // tell the two apart in the bar.
    h.fake_git().add_repo("/repo").staged_file("g.txt", "x\n");
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/f.txt"));
    h.settle_diff_jobs();
    h.snapshot();

    let buf = h.rendered_buffer();
    let rendered: String = (buf.area.y..buf.area.y + buf.area.height)
        .map(|y| {
            (buf.area.x..buf.area.x + buf.area.width)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    // The composited bar joins segments with the status-line hairline rule, so
    // the inter-token spaces read back as `─`. Normalize them to recover the
    // logical segment text.
    assert!(
        rendered
            .replace('─', " ")
            .contains("\u{271a} 1 staged \u{270e} 1 unstaged · \u{270e}1 \u{271a}1"),
        "statusline reports the repo's files and the focused file's hunks:\n{rendered}"
    );
}

/// The repo segment names the branch and reads `clean` for a tree with no
/// change, and it carries a `REB` chip while a rebase is paused. A merge in
/// progress carries a `MERGE` chip and the merge head's name, or its short
/// sha when no branch names it. A detached HEAD reads as its short sha, an
/// untracked file counts apart from the unstaged hunks, and the branch's
/// divergence from its upstream follows its name.
#[test]
fn statusline_names_the_branch_and_a_clean_tree() {
    let row = |branch: Option<&str>,
               rebasing: bool,
               untracked: bool,
               upstream: Option<(usize, usize)>,
               merge: Option<Option<&str>>| {
        let mut h = stoat::test_harness::TestHarness::with_size(80, 12);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_fs().insert_file(Path::new("/repo/f.txt"), b"a\n");
        {
            let mut repo = h.fake_git().add_repo("/repo");
            repo.commit("a1000000", &[("f.txt", "a\n")])
                .head_file("f.txt", "a\n")
                .index_file("f.txt", "a\n")
                .set_rebase_in_progress(rebasing);
            if let Some(branch) = branch {
                repo.set_head_branch(branch);
            }
            if untracked {
                repo.untracked("new.txt");
            }
            if let Some((ahead, behind)) = upstream {
                repo.upstream("origin/main", ahead, behind);
            }
            if let Some(head_name) = merge {
                repo.merge_in_progress("f00dfeed1234", head_name, "Merge");
            }
        }
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/f.txt"));
        h.settle_diff_jobs();
        let row = bar_row(&h.render_composited()).replace('\u{2500}', " ");
        row.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    assert_eq!(
        [
            row(Some("main"), false, false, None, None),
            row(Some("main"), true, false, None, None),
            row(None, false, true, None, None),
            row(Some("main"), false, false, Some((2, 1)), None),
            row(Some("main"), false, false, None, Some(Some("feature"))),
            row(Some("main"), false, false, None, Some(None)),
        ],
        [
            "NOR repo f.txt \u{2387} main \u{2502} \u{2713} clean 1:1",
            "NOR repo f.txt \u{2387} main REB \u{2502} \u{2713} clean 1:1",
            "NOR repo f.txt \u{2387} a100000 \u{2502} ? 1 untracked 1:1",
            "NOR repo f.txt \u{2387} main \u{2191}2 \u{2193}1 \u{2502} \u{2713} clean 1:1",
            "NOR repo f.txt \u{2387} main MERGE feature \u{2502} \u{2713} clean 1:1",
            "NOR repo f.txt \u{2387} main MERGE f00dfee \u{2502} \u{2713} clean 1:1",
        ],
    );
}

/// Under a range review the focused file's map reads approvals, so the bar
/// words its counts as approved and unapproved.
#[test]
fn statusline_words_a_review_s_counts_as_approvals() {
    let mut h = stoat::test_harness::TestHarness::with_size(100, 12);
    let root = PathBuf::from("/approvals");
    let path = root.join("f.txt");
    let (base, text) = ("a\nb\nc\nd\n", "a\nB\nc\nD\n");
    h.fake_fs().insert_file(&path, text.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    h.open_file(&path);
    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    let dm = DiffMap::from_structural_changes_staged(
        structural_diff::diff(base, text),
        Arc::new(base.to_string()),
        text,
        &std::iter::once(3..4).collect::<Vec<_>>(),
    )
    .with_mark_kind(MarkKind::Approved);
    h.stoat
        .active_workspace_mut()
        .install_test_diff_map(buffer_id, dm);
    h.snapshot();

    let rendered = h.rendered_text().replace('─', " ");
    assert_eq!(
        [
            rendered.contains("1 approved / 1 unapproved"),
            rendered.contains("1 staged / 1 unstaged"),
        ],
        [true, false],
        "the bar words the file's counts as approvals:\n{rendered}",
    );
}

/// A status message reports what the reader just did and leaves after a
/// few seconds, while the staged summary stays. So the summary gives way
/// while a message it leaves no room for is up. The two follow reports
/// differ only in their last word, which a cut to the leftover room drops.
///
/// The row is 55 cells wide, so the room left of the summary holds
/// `saved` and not `follow changes off`.
#[test]
fn the_staged_summary_gives_way_to_a_message_it_leaves_no_room_for() {
    for stoatty in [true, false] {
        let mut h = stoat::test_harness::TestHarness::with_size(55, 12);
        h.stoat.stoatty = stoatty;
        h.stage_index_scenario(
            "/repo",
            &[("f.txt", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n")],
        );
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/f.txt"));
        h.stoat
            .active_workspace_mut()
            .set_worktree_base(WorktreeBase::Index);
        h.settle_diff_jobs();

        let mut bar = |message: &str| {
            h.stoat.set_status(message);
            let row = bar_row(&h.render_composited()).replace('─', " ");
            row.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        assert_eq!(
            [bar("saved"), bar("follow changes off")],
            [
                "NOR repo f.txt saved \u{270e} 1 unstaged · \u{270e}1 \u{271a}1 1:1",
                "NOR repo f.txt follow changes off 1:1",
            ],
            "stoatty {stoatty}",
        );
    }
}

/// A changes view's status row reads the diff view's metrics with the
/// stack's file count in place of the screen's kind, and its pane row
/// through the focused excerpt. A walk puts the cursor on the first chunk,
/// so the hunk reads `1 / 1`.
#[test]
fn the_changes_view_bar_reads_the_focused_excerpts_hunks() {
    let mut h = stoat::test_harness::TestHarness::with_size(112, 12);
    h.stage_index_scenario(
        "/repo",
        &[("f.txt", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n")],
    );
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/f.txt"));
    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Index);
    h.cover_with_diff_changes(false);
    h.settle_diff_jobs();
    h.type_keys("] g");

    let row = bar_row(&h.render_composited()).replace('─', " ");
    let point = stoat::test_harness::editor::focused_cursor_point(&mut h.stoat);
    assert_eq!(
        row.split_whitespace().collect::<Vec<_>>().join(" "),
        format!(
            "vs index \u{b7} 1 file \u{b7} hunk 1 / 1 +1 \u{2212}1 NOR repo f.txt {}:{}",
            point.row + 1,
            point.column + 1,
        ),
    );
}

/// A changes view's status row counts the files its stack lists and sums
/// the lines their hunks add and remove against the divider. A line count
/// of zero takes no place.
#[test]
fn the_changes_view_bar_counts_the_stack_s_files_and_lines() {
    let bar = |files: &[(&str, &str, &str)]| {
        let mut h = stoat::test_harness::TestHarness::with_size(140, 12);
        h.stage_review_scenario("/repo", files);
        h.open_file(&Path::new("/repo").join(files[0].0));
        h.cover_with_diff_changes(false);
        h.settle();
        let row = bar_row(&h.render_composited()).replace('─', " ");
        row.split_whitespace().collect::<Vec<_>>().join(" ")
    };

    assert_eq!(
        [
            bar(&[
                ("a.txt", "a\nx\n", "A\nx\n"),
                ("b.txt", "b\nd\ne\n", "b\ne\n")
            ]),
            bar(&[("c.txt", "c\n", "c\nC\nD\n")]),
        ],
        [
            "vs HEAD \u{b7} 2 files \u{b7} hunk 1 / 2 +1 \u{2212}2 NOR repo a.txt 1:1",
            "vs HEAD \u{b7} 1 file \u{b7} hunk - / 1 +2 NOR repo c.txt 1:1",
        ],
    );
}

/// A changes view's status row counts each kind among its files that is no
/// plain modification, an untracked file as new, and the files folded to
/// their headers. The deleted file starts folded.
#[test]
fn a_changes_view_s_status_row_counts_its_files_by_kind() {
    let mut h = stoat::test_harness::TestHarness::with_size(200, 12);
    h.stoat.set_diff_warm_auto(true);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
    h.fake_git()
        .add_repo("/repo")
        .with_fs(h.fake_fs())
        .renamed("old.txt", "moved.txt", "same\n")
        .untracked("new.txt")
        .deleted("gone.txt", "gone\n");
    h.fake_fs()
        .insert_file(Path::new("/repo/new.txt"), b"fresh\n");
    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Head);
    h.open_file(Path::new("/repo/a.txt"));
    h.cover_with_diff_changes(false);
    h.settle_diff_jobs();

    let row = bar_row(&h.render_composited()).replace('─', " ");
    let lead =
        "vs HEAD \u{b7} 4 files \u{b7} 1 moved \u{b7} 1 new \u{b7} 1 deleted \u{b7} 1 collapsed";
    assert_eq!(
        row.split_whitespace().collect::<Vec<_>>().join(" "),
        format!("{lead} \u{b7} hunk 1 / 3 +2 \u{2212}2 NOR repo a.txt 1:1"),
    );
}

/// During a merge a changes view's status row names the merge after its
/// base, and counts the unmerged files and their conflict blocks after its
/// files. A new file that the merge brought counts as a file and under no
/// kind.
#[test]
fn a_changes_view_s_status_row_names_the_merge() {
    let block = |n: u32| format!("<<<<<<< HEAD\nours {n}\n=======\ntheirs {n}\n>>>>>>> main\n");
    let mut h = stoat::test_harness::TestHarness::with_size(200, 12);
    h.stoat.set_diff_warm_auto(true);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_git()
        .add_repo("/repo")
        .with_fs(h.fake_fs())
        .merge_in_progress("f00dfeed", Some("main"), "Merge branch 'main'")
        .modified("a.txt", "x\n", &format!("{}x\n{}", block(1), block(2)))
        .conflicted_file("a.txt", Some("x\n"), Some("ours\n"), Some("theirs\n"))
        .modified("b.txt", "y\n", &format!("{}y\n", block(3)))
        .conflicted_file("b.txt", Some("y\n"), Some("ours\n"), Some("theirs\n"))
        .head_file("c.txt", "c\n")
        .staged_file("c.txt", "C\n")
        .staged_file("d.txt", "d\n");
    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Head);
    h.open_file(Path::new("/repo/a.txt"));
    h.cover_with_diff_changes(false);
    h.settle_diff_jobs();

    let row = bar_row(&h.render_composited()).replace('─', " ");
    let lead = "vs HEAD \u{b7} merging main \u{b7} 4 files \u{b7} 2 conflicted \u{b7} 3 conflicts";
    assert_eq!(
        row.split_whitespace().collect::<Vec<_>>().join(" "),
        format!("{lead} \u{b7} hunk 1 / 5 +17 \u{2212}1 NOR repo a.txt 1:1"),
    );
}

/// A changes view's status row names the Scope of the cursor's row after
/// the file name, each part in its role's color. A row too narrow for the
/// whole chain drops items from the chain's middle, and the position keeps
/// its place.
#[test]
fn the_changes_view_bar_names_the_scope_under_the_cursor() {
    let base =
        "mod outer {\n    impl Theme {\n        fn panel() -> u32 {\n            1\n        \
                }\n    }\n}\n";
    let open = |width: u16| {
        let mut h = stoat::test_harness::TestHarness::with_size(width, 12);
        h.stage_review_scenario("/repo", &[("a.rs", base, &base.replacen('1', "2", 1))]);
        h.open_file(Path::new("/repo/a.rs"));
        h.cover_with_diff_changes(false);
        h.settle();
        // The paint asks for the parse, and the next paint reads it.
        h.render_composited();
        for _ in 0..2 {
            h.stoat.drive_background();
            h.settle();
        }
        h.type_keys("] g");
        let buf = h.render_composited();
        (h, buf)
    };
    let text = |buf: &Buffer| {
        let row = bar_row(buf).replace('─', " ");
        row.split_whitespace().collect::<Vec<_>>().join(" ")
    };

    let (wide, buf) = open(200);
    let y = buf.area.height - 1;
    let symbols: Vec<&str> = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
    let fg_at = |word: &str| {
        let x = symbols
            .windows(word.chars().count())
            .position(|cells| cells.concat() == word)?;
        Some(buf[(x as u16, y)].fg)
    };
    let theme = |scope: &str| wide.stoat.theme.get(scope).fg;

    let lead = "vs HEAD \u{b7} 1 file \u{b7} hunk 1 / 1 +1 \u{2212}1 NOR repo a.rs";
    assert_eq!(
        (
            text(&buf),
            text(&open(104).1),
            [fg_at("mod"), fg_at("outer"), fg_at("\u{203a}")],
        ),
        (
            format!("{lead} mod outer \u{203a} impl Theme \u{203a} fn panel 4:1"),
            format!("{lead} mod outer \u{203a} \u{2026} \u{203a} fn panel 4:1"),
            [
                theme(stoat::theme::scope::DIFF_SCOPE_KEYWORD),
                theme(stoat::theme::scope::DIFF_SCOPE_NAME),
                theme(stoat::theme::scope::DIFF_SCOPE_SEPARATOR),
            ],
        ),
    );
}

/// The changes view's status row reclaims the single-minimap band's width,
/// as an editor pane's row does, so the cells under the band carry the
/// row's target half.
#[test]
fn the_changes_view_bar_spans_the_minimap_band() {
    let mut h = stoat::test_harness::TestHarness::with_size(200, 24);
    h.stoat.settings.editor_minimap = Some(stoat_config::MinimapMode::Single);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
    h.open_file(Path::new("/repo/a.txt"));
    h.cover_with_diff_changes(false);
    let buf = h.stoat.render();

    let band = h
        .stoat
        .single_minimap_rect
        .expect("single mode reserves a band");
    let target = h
        .stoat
        .theme
        .get(stoat::theme::scope::UI_STATUSBAR_FOCUSED)
        .bg
        .expect("the theme fills the focused status row");
    let row = band.y + band.height;
    assert_eq!(
        (band.x..band.x + band.width)
            .map(|x| buf[(x, row)].bg)
            .collect::<Vec<_>>(),
        vec![target; usize::from(band.width)],
    );
}

/// Under islands no status row reaches the gap column or the band. The
/// islands on each side of that column reach into it, and the band's
/// island spans the rows of a pane island that spans the pane tree.
///
/// After `SplitRight` and `SplitDown` at 200 by 24, the left pane spans the
/// tree's rows, and two panes stack in the right column beside the band.
#[test]
fn the_band_island_spans_the_pane_rows_and_no_status_row_reaches_it() {
    use stoat_config::MinimapMode;
    use stoatty_protocol::{
        command::{Edge, PanelCommand},
        PROTOCOL_VERSION,
    };

    let mut h = stoat::test_harness::TestHarness::with_size(200, 24);
    h.stoat.stoatty_protocol = PROTOCOL_VERSION;
    h.stoat.settings.editor_minimap = Some(MinimapMode::Single);
    h.type_action("SplitRight()");
    h.type_action("SplitDown()");
    h.settle();
    h.stoat.render();

    let band = h
        .stoat
        .single_minimap_rect
        .expect("single mode reserves a band");
    let left = h
        .stoat
        .active_workspace()
        .panes
        .split_panes()
        .map(|(_, pane)| pane.area)
        .min_by_key(|area| area.x)
        .expect("the left pane");
    let ends: Vec<u16> = pane_status_rects(&h)
        .iter()
        .map(|row| row.right())
        .collect();
    let mut islands: Vec<PanelCommand> = h
        .stoat
        .apc_scene
        .panels()
        .filter(|panel| panel.backdrop)
        .collect();
    islands.sort_by_key(|panel| (panel.left, panel.top));
    let sides: Vec<(Edge, Edge)> = islands
        .iter()
        .map(|panel| (panel.edges[3], panel.edges[1]))
        .collect();
    let rows = |panel: &PanelCommand| (panel.top, panel.height, panel.edges[0], panel.edges[2]);

    let (margin, gap) = (Edge::Margin(8), Edge::Gap(8));
    assert_eq!(
        (ends, sides, islands.last().map(rows)),
        (
            vec![left.right(), band.x - 1, band.x - 1],
            vec![(margin, gap), (gap, gap), (gap, gap), (gap, margin)],
            islands.first().map(rows),
        ),
        "each status row ends with its pane, short of the gap column, the islands beside the \
         band reach into that column, and the band's island spans the left island's rows",
    );
}

/// A changes view whose stack lists no file still names the base it
/// compares against, and counts no file and no hunk. Its pane row names
/// the view, since no excerpt names a file.
#[test]
fn the_changes_view_bar_names_an_empty_stack_s_base() {
    let mut h = stoat::test_harness::TestHarness::with_size(112, 12);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_git().add_repo("/repo").head_file("a.txt", "a\n");
    h.fake_fs().insert_file("/repo/a.txt", "a\n");
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.txt"));
    h.cover_with_diff_changes(false);
    h.settle_diff_jobs();

    let row = bar_row(&h.render_composited()).replace('─', " ");
    assert_eq!(
        row.split_whitespace().collect::<Vec<_>>().join(" "),
        "vs HEAD NOR repo [diff-changes]",
    );
}

/// Recording takes every keypress until it is toggled off, and the status
/// bar is the only thing on screen that says so.
///
/// The indicator has to leave with the recording too. One that stays
/// claims the editor still takes keys it no longer takes.
#[test]
fn the_status_bar_shows_which_register_is_recording() {
    let mut h = stoat::test_harness::TestHarness::with_size(100, 12);
    h.seed_focused_buffer("hello");

    let bar_text = |h: &mut stoat::test_harness::TestHarness| -> String {
        h.snapshot();
        let buf = h.rendered_buffer();
        (buf.area.y..buf.area.y + buf.area.height)
            .map(|y| {
                (buf.area.x..buf.area.x + buf.area.width)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .replace('─', " ")
    };

    assert!(
        !bar_text(&mut h).contains("REC"),
        "nothing records yet, so nothing says so",
    );

    h.type_keys("\" a");
    h.type_keys("Q");
    let recording = bar_text(&mut h);
    assert!(
        recording.contains("REC a"),
        "the bar names the register being recorded into:\n{recording}",
    );

    h.type_keys("Q");
    let stopped = bar_text(&mut h);
    assert!(
        !stopped.contains("REC"),
        "and the indicator leaves with the recording:\n{stopped}",
    );
}

/// A command that waits on a buffer ends when the buffer leaves, and a quit
/// over the buffer returns to a shell, so the status row of each pane that
/// shows the buffer names what waits. The split shows the buffer in both
/// panes, so both rows name it.
///
/// The mark has to leave with the waiter while the file stays on screen.
#[test]
fn the_status_bar_names_what_waits_on_a_held_buffer() {
    let mut h = stoat::test_harness::TestHarness::with_size(160, 12);
    let path = PathBuf::from("/bridge/msg.txt");
    h.fake_fs().insert_file(&path, b"draft\n");
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/bridge");
    let uid = h.stoat.active_workspace().uid;
    let (hold, _outcome) = mpsc::unbounded_channel();
    h.stoat.handle_agent_control(AgentControl::Open {
        uid,
        client: 0,
        term: None,
        paths: vec![path],
        split: true,
        hold: Some(hold),
        done: oneshot::channel().0,
    });
    h.settle();
    dispatch(&mut h.stoat, &stoat_action::SplitRight);

    let names = |h: &mut stoat::test_harness::TestHarness| {
        h.snapshot();
        let text = h.rendered_text();
        (
            text.matches("msg.txt").count(),
            text.matches("msg.txt [for agent] ").count(),
        )
    };
    assert_eq!(
        names(&mut h),
        (2, 2),
        "each row over the file names its waiter"
    );

    h.stoat
        .handle_agent_control(AgentControl::ClientGone { uid, client: 0 });
    assert_eq!(names(&mut h), (2, 0), "the mark leaves with the waiter");
}

/// The search prompt is the only thing on screen that reports the query, so
/// a user without it types blind.
///
/// The sigil has to distinguish the two directions, and the prompt has to
/// leave when the search does. A bar still showing `/foo` claims a prompt
/// that no longer takes keys.
#[test]
fn the_status_bar_shows_the_search_query_while_it_is_typed() {
    let mut h = stoat::test_harness::TestHarness::with_size(100, 12);
    h.seed_focused_buffer("hello");

    let bar_text = |h: &mut stoat::test_harness::TestHarness| -> String {
        h.snapshot();
        h.rendered_text().replace('─', " ")
    };

    h.type_keys("/ f o o");
    let typing = bar_text(&mut h);
    assert!(
        typing.contains("/foo"),
        "the bar reads back the forward query:\n{typing}",
    );

    // The caret has to invert the bar's colors rather than carry a REVERSED
    // modifier, because the rich status path resolves a segment to a color
    // pair and drops its modifiers.
    let buf = h.rendered_buffer();
    let row = buf.area.height - 1;
    let sigil = (0..buf.area.width)
        .find(|&x| buf[(x, row)].symbol() == "/")
        .expect("the prompt paints on the status row");
    let (query, caret) = (&buf[(sigil, row)], &buf[(sigil + 4, row)]);
    assert_eq!(
        (caret.fg, caret.bg),
        (query.bg, query.fg),
        "the caret cell past the query inverts the bar's own colors",
    );

    h.type_keys("escape");
    let cancelled = bar_text(&mut h);
    assert!(
        !cancelled.contains("/foo"),
        "and the prompt leaves with the search:\n{cancelled}",
    );

    h.type_keys("g ?");
    h.type_text("b");
    let reverse = bar_text(&mut h);
    assert!(
        reverse.contains("?b"),
        "a reverse search prompts with its own sigil:\n{reverse}",
    );
}

/// The focused pane's editor, whichever input holds focus.
fn pane_editor(h: &stoat::test_harness::TestHarness) -> &EditorState {
    let ws = h.stoat.active_workspace();
    let View::Editor(id) = ws.panes.pane(ws.panes.focus()).view else {
        panic!("the focused pane shows an editor");
    };
    &ws.editors[id]
}

/// Whether `buf` paints text column `col` of the focused pane's first text
/// row as the theme's cursor block.
fn block_at(h: &stoat::test_harness::TestHarness, buf: &Buffer, col: u16) -> bool {
    let rect = pane_editor(h).text_rect.expect("the pane painted its text");
    let cell = &buf[(rect.x + col, rect.y)];
    let cursor = h.stoat.theme.cursor_style();
    cursor.bg.is_none_or(|bg| cell.bg == bg) && cell.modifier.contains(cursor.add_modifier)
}

/// Whether column zero paints as a block before a `/` search for `query`,
/// and whether the match at column `col` does while the prompt is open and
/// after Enter.
fn block_triple(h: &mut stoat::test_harness::TestHarness, query: &str, col: u16) -> [bool; 3] {
    h.snapshot();
    let before = block_at(h, h.rendered_buffer(), 0);
    h.type_keys("/");
    h.type_text(query);
    h.stoat.drive_background();
    h.snapshot();
    let during = block_at(h, h.rendered_buffer(), col);
    h.type_keys("enter");
    h.snapshot();
    [before, during, block_at(h, h.rendered_buffer(), col)]
}

/// The terminal draws the focused editor's primary cursor, so the pane
/// leaves that cell alone. A search prompt takes the terminal cursor, so
/// the previewed match paints its own block until the prompt closes.
#[test]
fn the_previewed_match_keeps_its_cursor_block_while_the_prompt_is_open() {
    let mut h = stoat::test_harness::TestHarness::with_size(40, 6);
    h.seed_focused_buffer("abc def");
    assert_eq!(block_triple(&mut h, "d", 4), [false, true, false]);
}

/// The diff view's right column takes the same cursor as a plain pane.
#[test]
fn a_diff_view_match_keeps_its_cursor_block_while_the_prompt_is_open() {
    let mut h = stoat::test_harness::TestHarness::with_size(80, 6);
    h.seed_focused_buffer("abc def");
    focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);
    assert_eq!(block_triple(&mut h, "d", 4), [false, true, false]);
}

/// The conflict view's center column takes the same cursor as a plain pane.
#[test]
fn a_conflict_view_match_keeps_its_cursor_block_while_the_prompt_is_open() {
    let mut h = stoat::test_harness::TestHarness::with_size(90, 12);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_git()
        .add_repo("/repo")
        .with_fs(h.fake_fs())
        .conflicted_file(
            "f.txt",
            Some("abc def\nbase\n"),
            Some("abc def\nours\n"),
            Some("abc def\ntheirs\n"),
        );
    dispatch(&mut h.stoat, &Conflict);
    h.settle();
    assert_eq!(block_triple(&mut h, "d", 4), [false, true, false]);
}

/// A prompt that opens over an insert-mode pane moves the terminal cursor
/// off the pane and leaves the mode as it was. With no background pass, no
/// preview runs either, so the terminal cursor is the one thing the next
/// frame changes, and the pane repaints its primary cell as a block rather
/// than replaying the paint that left the cell to the terminal.
#[test]
fn a_terminal_cursor_moving_off_the_pane_repaints_it() {
    let mut h = stoat::test_harness::TestHarness::with_size(40, 6);
    h.seed_focused_buffer("abc def");
    h.type_keys("i");
    let delegated = h.stoat.render();
    dispatch(&mut h.stoat, &OpenSearchInput);
    let painted = h.stoat.render();
    assert_eq!(
        [block_at(&h, &delegated, 0), block_at(&h, &painted, 0)],
        [false, true],
    );
}

/// A bottom-right badge draws its chip on the status row, and whatever the
/// bar put under the chip is painted over. A message loses its end there,
/// and the cursor position goes entirely.
///
/// The check runs for the rich bar under stoatty and for the cell fallback
/// that a foreign terminal gets.
#[test]
fn the_right_segments_end_where_a_corner_badge_starts() {
    for stoatty in [true, false] {
        let mut h = stoat::test_harness::TestHarness::with_size(100, 12);
        h.stoat.stoatty = stoatty;
        h.seed_focused_buffer("hello");
        h.snapshot();
        h.stoat.set_status("walkthrough 'empty' has no stops");
        corner_chip(&mut h);

        let buf = h.render_composited();

        // The 14-wide chip sits one column in from the right edge, so it
        // starts at column 85.
        let row = buf.area.height - 1;
        let expected = "walkthrough 'empty' has no stops 1:1 ";
        let start = 85 - expected.chars().count() as u16;
        let beside_the_box: String = (start..85)
            .map(|x| buf[(x, row)].symbol())
            .collect::<String>()
            .replace('─', " ");
        assert_eq!(beside_the_box, expected, "stoatty {stoatty}");
    }
}

/// A message longer than the room left of the badge is cut to that room.
/// Cut to the room the whole row gives, it runs back over the mode and
/// workspace segments in the cells, and the rich bar drops it for the
/// overlap.
#[test]
fn a_long_message_is_cut_where_the_badge_starts() {
    for stoatty in [true, false] {
        let mut h = stoat::test_harness::TestHarness::with_size(60, 12);
        h.stoat.stoatty = stoatty;
        h.seed_focused_buffer("hello");
        h.snapshot();
        h.stoat.set_status("walkthrough 'empty' has no stops");
        corner_chip(&mut h);

        let buf = h.render_composited();

        // The 14-wide chip starts at column 45 of 60.
        let row = buf.area.height - 1;
        let left_of_the_box: String = (0..45)
            .map(|x| buf[(x, row)].symbol())
            .collect::<String>()
            .replace('─', " ");
        assert_eq!(
            left_of_the_box, " NOR  (unnamed) scratch [+] walkthrough  1:1 ",
            "stoatty {stoatty}",
        );
    }
}

/// Put a 14-wide chip at the bottom right, from a 12-character label of a
/// source no frame sync takes down.
fn corner_chip(h: &mut stoat::test_harness::TestHarness) {
    h.stoat.active_workspace_mut().badges.insert(Badge {
        source: BadgeSource::Review,
        anchor: Anchor::BottomRight,
        state: BadgeState::Complete,
        label: "reviewed 1/3".to_owned(),
        detail: None,
    });
}

/// A compressed buffer's status row names the file's size on disk and its
/// text's size after the name.
#[test]
fn the_status_row_names_a_gzip_buffer_s_sizes() {
    let mut h = stoat::test_harness::TestHarness::with_size(60, 12);
    let root = PathBuf::from("/ro");
    let path = root.join("a.txt.gz");
    let gz = stoat::test_harness::gzip(b"hello\n");
    h.fake_fs().insert_file(&path, gz.clone());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    // A gzip file reads on the blocking pool, and the read installs here.
    buffer_lifecycle::install_pending_opens(&mut h.stoat);
    h.settle();

    let buf = h.render_composited();

    let row = buf.area.height - 1;
    let bar = (0..buf.area.width)
        .map(|x| buf[(x, row)].symbol())
        .collect::<String>()
        .replace('─', " ");
    let lead = format!(" NOR  ro a.txt.gz  gzip \u{b7} {} B \u{2192} 6 B", gz.len());
    let kind_x = lead.find("gzip").expect("the lead names the kind") as u16;
    let muted = h
        .stoat
        .theme
        .get(stoat::theme::scope::UI_TEXT_MUTED)
        .fg
        .expect("the theme mutes text");
    assert_eq!(
        (bar, buf[(kind_x, row)].fg),
        (format!("{lead:<56}1:1 "), muted),
        "the sizes after the name, in the muted color",
    );
}

/// A buffer the process lacks write access to reads `name [+]  read only`,
/// its kind in a muted segment after the modified tag.
#[test]
fn the_status_row_tags_a_read_only_buffer() {
    let mut h = stoat::test_harness::TestHarness::with_size(60, 12);
    let root = PathBuf::from("/ro");
    let path = root.join("file.txt");
    h.fake_fs().insert_file(&path, b"text\n");
    h.fake_fs().set_read_only(&path, true);
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    h.edit_focused(4..4, "!");

    let buf = h.render_composited();

    let row = buf.area.height - 1;
    let bar = (0..buf.area.width)
        .map(|x| buf[(x, row)].symbol())
        .collect::<String>()
        .replace('─', " ");
    assert_eq!(
        bar,
        format!("{:<56}1:1 ", " NOR  ro file.txt [+]  read only")
    );
}

/// The focused conflict view's row splits at the MergeGrid's merged
/// column, as the paint records it. At 160 cells the pane spans 153 beside
/// the minimap band, so the merged column takes cells 49 to 104, and the
/// theirs third runs on under the band.
///
/// The ours third reads what merges into what, the file's place, and the
/// chunk at the cursor in the muted color. The open chunks fill its last
/// cells against the divide, in the conflict header's color. The merged
/// third is the pane row, with the mode chip at the column's first cell
/// and the conflicted file in place of the scratch buffer. The theirs third
/// names theirs in the muted color.
#[test]
fn a_conflict_row_splits_in_thirds_under_the_grid_columns() {
    let mut h = stoat::test_harness::TestHarness::with_size(160, 12);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_git()
        .add_repo("/repo")
        .with_fs(h.fake_fs())
        .set_head_branch("main")
        .merge_in_progress("a1c4e9f00", Some("topic"), "Merge branch 'topic'")
        .conflicted_file(
            "src/f.txt",
            Some("a\nb\nc\nd\ne\n"),
            Some("a\nB\nc\nD\ne\n"),
            Some("a\nX\nc\nY\ne\n"),
        );
    dispatch(&mut h.stoat, &Conflict);
    h.settle();

    let buf = h.render_composited();
    let layout = focused_editor_mut(&mut h.stoat).and_then(|editor| editor.conflict_layout.clone());
    let theme = &h.stoat.theme;
    let fg = |scope: &str| theme.get(scope).fg.expect("the theme colors the scope");
    let muted = muted_tint(theme.get(stoat::theme::scope::UI_STATUSBAR_FOCUSED), theme)
        .fg
        .expect("the row mutes text");
    let row = buf.area.height - 1;
    let facts = " topic \u{2192} main \u{b7} file 1 / 1 \u{b7} chunk 1 / 2 ";
    assert_eq!(
        (
            layout,
            status_row(&buf, buf.area),
            [1, 42, 50, 105].map(|x| buf[(x, row)].fg),
        ),
        (
            Some(ConflictLayout {
                ours: 0..49,
                merged: 49..104,
                theirs: 104..153,
                body: Rect::new(0, 2, 153, 9),
                chips: vec![(Rect::new(85, 3, 19, 1), 0), (Rect::new(85, 9, 19, 1), 1)],
                buttons: Vec::new(),
            }),
            format!(
                "{facts} 2 left {:<51}2:1 {:<56}",
                " NOR  repo src/f.txt", " topic \u{b7} a1c4e9f"
            ),
            [
                muted,
                fg(stoat::theme::scope::VCS_CONFLICT_HEADER),
                fg(stoat::theme::scope::UI_MODE_LABEL),
                muted,
            ],
        ),
    );
}

/// A status message on a conflict row takes the room that the merged third
/// leaves between its file and its position. The row cuts the message to
/// that room, as a pane row cuts its message to the row's room.
#[test]
fn a_message_on_a_conflict_row_fits_the_merged_third() {
    let mut h = stoat::test_harness::TestHarness::with_size(160, 12);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_git()
        .add_repo("/repo")
        .with_fs(h.fake_fs())
        .conflicted_file(
            "f.txt",
            Some("a\nbase\n"),
            Some("a\nours\n"),
            Some("a\ntheirs\n"),
        );
    dispatch(&mut h.stoat, &Conflict);
    h.settle();
    h.stoat
        .set_status("resolution written to the working tree for f.txt");

    let buf = h.render_composited();
    let facts = " theirs \u{2192} HEAD \u{b7} file 1 / 1 \u{b7} chunk 1 / 1";
    assert_eq!(
        status_row(&buf, buf.area),
        format!(
            "{facts:<49} NOR  repo f.txt resolution written to the working 2:1 {:<56}",
            " theirs"
        ),
    );
}

/// With no MergeGrid, as a terminal with no live scene paints the conflict
/// view, the focused conflict row runs whole. It names the conflicted file
/// and then reads what merges into what and the chunk at the cursor in the
/// muted color.
#[test]
fn a_conflict_row_with_no_grid_reads_the_merge_after_the_file() {
    let mut h = stoat::test_harness::TestHarness::with_size(60, 12);
    h.stoat.stoatty = false;
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_git()
        .add_repo("/repo")
        .with_fs(h.fake_fs())
        .conflicted_file(
            "f.txt",
            Some("a\nbase\n"),
            Some("a\nours\n"),
            Some("a\ntheirs\n"),
        );
    dispatch(&mut h.stoat, &Conflict);
    h.settle();

    let buf = h.render_composited();
    let layout = focused_editor_mut(&mut h.stoat).and_then(|editor| editor.conflict_layout.clone());
    let lead = " NOR  repo f.txt  theirs \u{2192} HEAD \u{b7} chunk 1 / 1";
    let at = lead.find("theirs").expect("the lead names theirs") as u16;
    let theme = &h.stoat.theme;
    let muted = muted_tint(theme.get(stoat::theme::scope::UI_STATUSBAR_FOCUSED), theme).fg;
    assert_eq!(
        (
            layout,
            status_row(&buf, buf.area),
            Some(buf[(at, buf.area.height - 1)].fg)
        ),
        (None, format!("{lead:<56}2:1 "), muted),
    );
}

/// The conflict pane's status row splits in thirds and reads the merge
/// while the conflict view holds focus. Once focus moves to the pane beside
/// it, that pane's row leads with the mode chip and its file. The conflict
/// pane's row runs whole and names its scratch buffer, since only the
/// focused conflict pane reads the merge. The open widens its pane, so the
/// layout unwidens to show the other one.
///
/// At 320 cells the conflict pane's row takes 163, of which the view spans
/// 156 beside the minimap band. So the thirds take 50, 56, and 57 cells.
#[test]
fn the_merge_facts_leave_the_row_with_the_focus() {
    let mut h = stoat::test_harness::TestHarness::with_size(320, 12);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_git()
        .add_repo("/repo")
        .with_fs(h.fake_fs())
        .conflicted_file(
            "f.txt",
            Some("a\nbase\n"),
            Some("a\nours\n"),
            Some("a\ntheirs\n"),
        );
    dispatch(
        &mut h.stoat,
        &OpenFile {
            path: PathBuf::from("/repo/f.txt"),
        },
    );
    h.settle();
    dispatch(&mut h.stoat, &stoat_action::SplitRight);
    dispatch(&mut h.stoat, &Conflict);
    h.settle();
    h.stoat.active_workspace_mut().panes.unwiden();

    let rows = |h: &mut stoat::test_harness::TestHarness| {
        let buf = h.render_composited();
        pane_status_rows(h, &buf)
    };
    let on_conflict = rows(&mut h);
    {
        let panes = &mut h.stoat.active_workspace_mut().panes;
        let focused = panes.focus();
        let beside = panes
            .split_pane_ids()
            .into_iter()
            .find(|&id| id != focused)
            .expect("the pane beside the conflict view");
        panes.set_focus(beside);
    }
    let on_file = rows(&mut h);

    let [left, right] = [on_conflict[0].0, on_conflict[1].0];
    let facts = " theirs \u{2192} HEAD \u{b7} file 1 / 1 \u{b7} chunk 1 / 1 ";
    let thirds = format!(
        "{facts} 1 left {:<52}2:1 {:<57}",
        " NOR  repo f.txt", " theirs"
    );
    assert_eq!(
        [on_conflict, on_file],
        [
            vec![pane_row(left, " f.txt", "1:1"), (right, thirds)],
            vec![
                pane_row(left, " NOR  repo f.txt", "1:1"),
                pane_row(right, " scratch", "2:1"),
            ],
        ],
    );
}

/// The bottom row of `area` in `buf`, with the composite's blank bar cells
/// read as spaces.
fn status_row(buf: &Buffer, area: Rect) -> String {
    let row = area.y + area.height - 1;
    (area.x..area.x + area.width)
        .map(|x| buf[(x, row)].symbol())
        .collect::<String>()
        .replace('─', " ")
}

/// The split panes' status rows in `buf`, in [`pane_status_rects`] order,
/// each as its width and its text as [`status_row`] reads it.
fn pane_status_rows(h: &stoat::test_harness::TestHarness, buf: &Buffer) -> Vec<(u16, String)> {
    pane_status_rects(h)
        .into_iter()
        .map(|row| (row.width, status_row(buf, row)))
        .collect()
}

/// The split panes' status rows, left to right and then top to bottom.
fn pane_status_rects(h: &stoat::test_harness::TestHarness) -> Vec<Rect> {
    let mut areas: Vec<Rect> = h
        .stoat
        .active_workspace()
        .panes
        .split_panes()
        .map(|(_, pane)| pane.area)
        .collect();
    areas.sort_by_key(|area| (area.x, area.y));
    areas
        .into_iter()
        .map(|area| {
            stoat::render::pane_status_row(
                area,
                h.stoat.single_minimap_rect,
                h.stoat.size(),
                h.stoat.status_rows(),
            )
            .expect("the pane has a status row")
        })
        .collect()
}

/// A status row `width` cells wide that leads with `lead` and ends in the
/// position `at` and one pad cell.
fn pane_row(width: u16, lead: &str, at: &str) -> (u16, String) {
    let fill = usize::from(width) - at.len() - 1;
    (width, format!("{lead:<fill$}{at} "))
}

/// A revision buffer's status row names the working file as a file's row
/// does, and its commit after it.
#[test]
fn the_status_row_names_a_revision_file_and_its_commit() {
    let mut h = stoat::test_harness::TestHarness::with_size(60, 12);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    let path = Path::new("/repo/src/a.rs");
    buffer_lifecycle::show_revision(&mut h.stoat, path, "a1b2c3d4e5", "fn a() {}\n");

    let buf = h.render_composited();
    let rows = pane_status_rows(&h, &buf);
    assert_eq!(
        rows,
        [pane_row(
            rows[0].0,
            " NOR  repo src/a.rs  at a1b2c3d",
            "1:1"
        )],
    );
}

/// Several cursors show as `{count} sel` after the file name, and a lone
/// cursor shows no count.
#[test]
fn the_status_row_counts_the_cursors() {
    let mut h = stoat::test_harness::TestHarness::with_size(60, 12);
    h.seed_focused_buffer("foo\nbar\n");

    dispatch(&mut h.stoat, &AddSelectionBelow);
    let two = {
        let buf = h.render_composited();
        status_row(&buf, buf.area)
    };
    dispatch(&mut h.stoat, &KeepPrimarySelection);
    let buf = h.render_composited();
    assert_eq!(
        [two, status_row(&buf, buf.area)],
        [
            format!("{:<56}2:1 ", " NOR  (unnamed) scratch [+]  2 sel "),
            format!("{:<56}2:1 ", " NOR  (unnamed) scratch [+]"),
        ],
    );
}

/// An open picker shows the prompt chip and its own name where a screen
/// label sits, with no file name after it. A closed picker gives the row
/// back to the mode and the file. The search input has no name, so its row
/// keeps the mode chip.
#[test]
fn the_status_row_names_an_open_picker() {
    let mut h = stoat::test_harness::TestHarness::with_size(60, 12);
    h.seed_focused_buffer("foo\nbar\n");
    let row = |h: &mut stoat::test_harness::TestHarness| {
        let buf = h.render_composited();
        status_row(&buf, buf.area)
    };

    h.type_keys("space p");
    let open = row(&mut h);
    h.type_keys("escape");
    let closed = row(&mut h);
    dispatch(&mut h.stoat, &OpenSearchInput);
    assert_eq!(
        [open, closed, row(&mut h)],
        [
            format!("{:<56}1:1 ", " PMT  (unnamed)  find file"),
            format!("{:<56}1:1 ", " NOR  (unnamed) scratch [+]"),
            format!("{:<56}1:1 ", " INS /"),
        ],
    );
}

/// A focused overlay bar names an open picker in place of its screen, and
/// names the screen again once the picker closes. An unfocused bar shows no
/// prompt chip, so it keeps its screen under an open picker.
#[test]
fn the_overlay_bar_names_an_open_picker() {
    let mut h = stoat::test_harness::TestHarness::with_size(60, 12);
    h.stoat.active_workspace_mut().rebase = Some(RebaseState::new(
        PathBuf::from("/overlay"),
        "onto".into(),
        vec![],
    ));
    let row = |h: &mut stoat::test_harness::TestHarness| {
        let buf = h.render_composited();
        status_row(&buf, buf.area)
    };

    h.type_keys(":");
    let open = row(&mut h);
    h.type_keys("escape");
    let closed = row(&mut h);

    h.type_keys(":");
    let ws = h.stoat.active_workspace_mut();
    let dock = ws.docks.insert(DockPanel {
        view: View::Label("dock".into()),
        side: DockSide::Right,
        visibility: DockVisibility::Hidden,
        default_width: 30,
        area: Rect::default(),
    });
    ws.focus = FocusTarget::Dock(dock);
    assert_eq!(
        [open, closed, row(&mut h)],
        [
            format!("{:<60}", " PMT  (unnamed) palette"),
            format!("{:<60}", " NOR  (unnamed) rebase"),
            format!("{:<60}", " rebase"),
        ],
    );
}

/// The overlay bar names the workspace until a second tab shows the tab
/// bar, which then carries the name. The screen label then meets the mode
/// chip across a gap cell.
#[test]
fn the_overlay_bar_leaves_the_workspace_name_to_the_tab_bar() {
    let mut h = stoat::test_harness::TestHarness::with_size(60, 12);
    let ws = h.stoat.active_workspace_mut();
    ws.name = "ws".to_string();
    ws.rebase = Some(RebaseState::new(
        PathBuf::from("/overlay"),
        "onto".into(),
        vec![],
    ));
    let row = |h: &mut stoat::test_harness::TestHarness| {
        let buf = h.render_composited();
        status_row(&buf, buf.area)
    };

    let one_tab = row(&mut h);
    dispatch(&mut h.stoat, &stoat_action::NewTab);
    assert_eq!(
        [one_tab, row(&mut h)],
        [
            format!("{:<60}", " NOR  ws rebase"),
            format!("{:<60}", " NOR  rebase"),
        ],
    );
}

/// A focused commits bar names the selected commit after its screen, and a
/// merge's count of parents after that. An open picker takes the label
/// whole, with no commit after it.
#[test]
fn the_commits_bar_names_its_commit_and_an_open_picker_alone() {
    let mut h = stoat::test_harness::TestHarness::with_size(60, 12);
    h.fake_git()
        .add_repo("/repo")
        .commit_with_message("a1000000", "feat: root", &[])
        .commit_with_parent_message("b2000000", "a1000000", "feat: side", &[])
        .commit_with_parent_message("c3000000", "a1000000", "feat: main", &[])
        .merge_commit("d4000000", "b2000000", &[]);
    h.open_commits("/repo");
    let row = |h: &mut stoat::test_harness::TestHarness| {
        let buf = h.render_composited();
        status_row(&buf, buf.area)
    };

    let merge = row(&mut h);
    h.type_keys("j");
    let plain = row(&mut h);
    h.type_keys(":");
    assert_eq!(
        [merge, plain, row(&mut h)],
        [
            format!(
                "{:<60}",
                " NOR  repo commits \u{b7} d400000 merge \u{b7} 2 parents"
            ),
            format!("{:<60}", " NOR  repo commits \u{b7} c300000"),
            format!("{:<60}", " PMT  repo palette"),
        ],
    );
}

/// Each pane's status row counts that pane's cursors, and the mode chip
/// leads the focused pane's row alone, so the chip follows the focus while
/// each count stays with its pane.
#[test]
fn each_pane_s_status_row_counts_its_own_cursors() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 12);
    h.seed_focused_buffer("foo\nbar\n");
    dispatch(&mut h.stoat, &MoveDown);
    dispatch(&mut h.stoat, &AddSelectionAbove);
    let counted = h.stoat.active_workspace().panes.focus();
    h.type_action("SplitRight()");
    dispatch(&mut h.stoat, &KeepPrimarySelection);
    let rows = |h: &mut stoat::test_harness::TestHarness| {
        let buf = h.render_composited();
        pane_status_rows(h, &buf)
    };

    let away = rows(&mut h);
    h.stoat.active_workspace_mut().panes.set_focus(counted);
    let back = rows(&mut h);
    let [left, right] = [away[0].0, away[1].0];
    assert_eq!(
        [away, back],
        [
            vec![
                pane_row(left, " scratch [+]  2 sel ", "1:1"),
                pane_row(right, " NOR  (unnamed) scratch [+] ", "1:1"),
            ],
            vec![
                pane_row(left, " NOR  (unnamed) scratch [+]  2 sel ", "1:1"),
                pane_row(right, " scratch [+] ", "1:1"),
            ],
        ],
    );
}

/// The tab bar ends in the `files` and `changes` chips while a diff view or
/// a changes view has the focus. It shows neither over a plain editor, over
/// a diff whose split pane gave the focus to a dock, or where the chips do
/// not fit beside the tabs.
#[test]
fn the_tab_bar_names_the_diff_layouts_over_a_diff() {
    let mut h = toggles_harness();
    h.stoat.stoatty = false;
    let bar = |h: &mut stoat::test_harness::TestHarness| {
        h.snapshot();
        let text = h.rendered_text();
        let row = text.lines().next().unwrap_or_default();
        row.split_whitespace().collect::<Vec<_>>().join(" ")
    };

    let plain = bar(&mut h);
    focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);
    let diff = bar(&mut h);

    let ws = h.stoat.active_workspace_mut();
    let dock = ws.docks.insert(DockPanel {
        view: View::Label("dock".into()),
        side: DockSide::Right,
        visibility: DockVisibility::Hidden,
        default_width: 30,
        area: Rect::default(),
    });
    ws.focus = FocusTarget::Dock(dock);
    let docked = bar(&mut h);
    h.stoat.active_workspace_mut().focus = FocusTarget::SplitPane;

    h.resize(30, 8);
    let narrow = bar(&mut h);
    h.resize(120, 8);
    h.cover_with_diff_changes(false);
    h.settle();
    let changes = bar(&mut h);

    assert_eq!(
        [plain, diff, docked, narrow, changes],
        [
            "repo a.rs",
            "repo \u{b1} a.rs files changes",
            "repo \u{b1} a.rs",
            "repo \u{b1} a.rs",
            "repo \u{b1} changes \u{b7} 1 file files changes",
        ],
    );
}

/// A diff view marks the `files` chip active, and a changes view marks the
/// `changes` chip.
#[test]
fn diff_view_tab_toggles() {
    let mut h = toggles_harness();
    focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);
    h.assert_snapshot("diff_view_tab_toggles");

    h.cover_with_diff_changes(false);
    h.settle();
    h.assert_snapshot("changes_view_tab_toggles");
}

/// Under a live scene the focused layout's chip draws as a chip panel in its
/// fill, and no bar in that fill backs either chip. The pad ends the bar in
/// column 119, the `changes` chip takes the nine columns before it, and the
/// `files` chip the seven before those.
#[test]
fn the_live_tab_bar_boxes_the_focused_layout_s_chip() {
    use stoat::render::paint::style_rgb;
    use stoatty_protocol::command::{Command, Edge};

    let mut h = toggles_harness();
    let fill = style_rgb(
        h.stoat
            .theme
            .get(stoat::theme::scope::UI_TABBAR_TOGGLE_ACTIVE)
            .bg,
    )
    .expect("the theme fills the active chip in RGB");
    let chips = |h: &mut stoat::test_harness::TestHarness| {
        h.render_composited();
        let scene = stoatty_protocol::command::decode_stream(&h.stoat.apc_scene.all_bytes());
        let panels: Vec<_> = scene
            .iter()
            .filter_map(|command| match command {
                Command::Panel(panel) if panel.top == 0 && panel.fill == Some(fill) => {
                    Some((panel.left, panel.width, panel.edges))
                },
                _ => None,
            })
            .collect();
        let bars = scene
            .iter()
            .filter(
                |command| matches!(command, Command::Bar(bar) if bar.y < 16 && bar.color == fill),
            )
            .count();
        (panels, bars)
    };

    focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);
    let diff = chips(&mut h);
    h.cover_with_diff_changes(false);
    h.settle();
    let changes = chips(&mut h);

    let inset = Edge::symmetric(0, 1);
    assert_eq!(
        (diff, changes),
        ((vec![(103, 7, inset)], 0), (vec![(110, 9, inset)], 0)),
        "the files chip's panel over the diff and the changes chip's over the changes view, \
         and no bar in the chip's fill",
    );
}

/// With a Job the layout chips pack from the start of the tray rather than
/// from the bar's end. They drop where the tabs leave them no room before
/// the tray, though the bar past the tray would hold them.
#[test]
fn the_layout_chips_end_left_of_the_tray() {
    use stoatty_protocol::command::Command;

    let mut h = toggles_harness();
    dispatch(&mut h.stoat, &stoat_action::NewTab);
    h.push_job("sleep 30");
    dispatch(&mut h.stoat, &stoat_action::PrevTab);
    h.stoat.active_workspace_mut().name = "ws".to_string();
    focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);
    let fills = [
        stoat::render::paint::style_rgb(
            h.stoat
                .theme
                .get(stoat::theme::scope::UI_TABBAR_TOGGLE_ACTIVE)
                .bg,
        ),
        Some([0x1e, 0x21, 0x27]),
    ];
    let boxes = |h: &mut stoat::test_harness::TestHarness| -> Vec<(u16, u16)> {
        h.render_composited();
        stoatty_protocol::command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .filter_map(|command| match command {
                Command::Panel(panel) if panel.top == 0 && fills.contains(&panel.fill) => {
                    Some((panel.left, panel.width))
                },
                _ => None,
            })
            .collect()
    };

    let roomy = boxes(&mut h);
    let tabs_end = h.stoat.tab_bar_spans.last().map_or(0, |span| span.end / 16);
    h.stoat.active_workspace_mut().name = format!("ws{}", "x".repeat(usize::from(95 - tabs_end)));
    let crowded = boxes(&mut h);

    assert_eq!(
        (roomy, crowded),
        (vec![(85, 7), (102, 17)], vec![(102, 17)]),
        "the files chip packs from the tray's start, and drops once the tabs reach 95 \
         columns",
    );
}

/// One changed `a.rs` open in an editor, 120 by 8, with the tab bar on and
/// no minimap.
fn toggles_harness() -> stoat::test_harness::TestHarness {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 8);
    h.stoat.settings.ui_tab_bar = Some(stoat_config::TabBarMode::Always);
    h.stoat.minimap_override = Some(false);
    // The diff view keeps its hints box up, and 8 rows leave it over the
    // tab bar.
    h.stoat.screen_hints_hidden = true;
    h.stage_review_scenario("/repo", &[("a.rs", "a\nb\nc\nold\n", "a\nb\nc\nnew\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.rs"));
    h.settle_diff_jobs();
    h
}

/// The bar leads with the workspace name in its own color and a one-cell
/// gap. A tagged file's tab reads its stem and then its extension as a
/// suffix in the tag's color, and a gzip file's suffix holds both
/// extensions. Each tab's click span covers its label and its suffix.
#[test]
fn the_tab_bar_leads_with_the_workspace_and_tags_each_file() {
    use ratatui::style::Color;
    use stoat::render::paint::style_rgb;

    let mut h = tagged_tabs_harness();
    h.stoat.stoatty = false;
    h.snapshot();

    let text = h.rendered_text();
    let bar = text.lines().next().expect("the tab bar row");
    let lead = " ws   chrome.rs  panel.rs  CHANGELOG.md.gz ";
    let fg_at = |needle: &str| {
        let col = lead[..lead.find(needle).expect("the bar names it")]
            .chars()
            .count();
        h.rendered_buffer()[(col as u16, 0)].fg
    };
    let theme = &h.stoat.theme;
    let md = style_rgb(theme.get(stoat::theme::scope::UI_TAG_MARKDOWN).fg)
        .expect("the theme colors it in RGB");
    assert_eq!(
        (bar.trim_end(), Some(fg_at("ws")), fg_at(".md.gz")),
        (
            lead.trim_end(),
            theme.get(stoat::theme::scope::UI_TABBAR_NAME).fg,
            Color::Rgb(md[0], md[1], md[2]),
        ),
        "the bar text, the workspace name in its color, then the active gzip tab's suffix \
         in the markdown color",
    );
    assert_eq!(
        h.stoat.tab_bar_spans,
        [(5, 16), (16, 26), (26, 43)].map(|(start, end): (u16, u16)| start * 16..end * 16),
        "each tab's span in sixteenths, from its label's lead space to its suffix",
    );
}

/// Under a live scene a hairline parts the workspace name from the tabs, and
/// each suffix draws in its tag's color at the label's Ui type scale.
///
/// The active tab draws as one chip in its background, rounded at 4 and one
/// logical pixel inside the row's top and bottom, over the tab's cells.
///
/// The cells match the plain bar, since every role scales to a whole cell
/// under `cfg(test)`. The name ends at column 4, where the hairline stands
/// 12 sixteenths tall from sixteenth 2, and the bar draws no other mark.
#[test]
fn the_live_tab_bar_divides_the_name_boxes_the_active_tab_and_colors_each_suffix() {
    use ratatui::style::Color;
    use stoat::{render::paint::style_rgb, theme::TypeRole};
    use stoatty_protocol::command::{BorderStyle, Command, Edge, PanelCommand, PanelShadow};

    let mut h = tagged_tabs_harness();
    h.render_composited();

    let theme = &h.stoat.theme;
    let rgb = |color: Option<Color>| style_rgb(color).expect("the theme colors it in RGB");
    let active = rgb(theme.get(stoat::theme::scope::UI_TABBAR_ACTIVE).bg);
    let rust = rgb(theme.get(stoat::theme::scope::UI_TAG_RUST).fg);
    let md = rgb(theme.get(stoat::theme::scope::UI_TAG_MARKDOWN).fg);
    let divider = rgb(theme.get(stoat::theme::scope::UI_BORDER_INACTIVE).fg);
    let tab = PanelCommand {
        top: 0,
        left: 26,
        width: 17,
        height: 1,
        style: BorderStyle::None_,
        border: active,
        corner_radius: 4,
        fill: Some(active),
        shadow: PanelShadow::None_,
        edges: Edge::symmetric(0, 1),
        corners: 0,
        fillets: 0,
        fillet_radius: 2,
        hatch: None,
        above_pools: false,
        backdrop: false,
        anchor: None,
    };

    let scene = stoatty_protocol::command::decode_stream(&h.stoat.apc_scene.all_bytes());
    let panels: Vec<PanelCommand> = scene
        .iter()
        .filter_map(|command| match command {
            Command::Panel(panel) if panel.top == 0 => Some(*panel),
            _ => None,
        })
        .collect();
    let marks: Vec<_> = scene
        .iter()
        .filter_map(|command| match command {
            Command::Bar(bar) if bar.y < 16 && (2..16).contains(&bar.height) => {
                Some((bar.x, bar.y, bar.width, bar.height, bar.color))
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        (panels, marks),
        (vec![tab], vec![(64, 2, 1, 12, divider)]),
        "the active tab's chip, and the name's hairline alone",
    );

    let ui = theme.type_scale(TypeRole::Ui);
    let suffixes: Vec<(&str, u16, [u8; 3])> = scene
        .iter()
        .filter_map(|command| match command {
            Command::TextRun(run)
                if run.row < 16 && [".rs ", ".md.gz "].contains(&run.text.as_str()) =>
            {
                Some((run.text.as_str(), run.scale, run.color))
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        suffixes,
        [(".rs ", ui, rust), (".rs ", ui, rust), (".md.gz ", ui, md)],
        "each suffix's run carries the label's scale and its tag's color",
    );
}

/// The tab bar gives every tab one box, the active tab's a panel.
#[test]
fn the_tab_segments_box_each_tab() {
    use stoat::render::pane::{tab_segments, BoxShape};

    let h = tagged_tabs_harness();
    let theme = &h.stoat.theme;
    let styles = (
        theme.get(stoat::theme::scope::UI_TABBAR_ACTIVE),
        theme.get(stoat::theme::scope::UI_TABBAR_INACTIVE),
    );
    let segments = tab_segments(h.stoat.active_workspace(), theme, "ws", styles);
    let boxes: Vec<_> = segments
        .boxes
        .iter()
        .map(|run_box| (run_box.segments.clone(), run_box.shape))
        .collect();

    assert_eq!(
        boxes,
        vec![(2..4, None), (4..6, None), (6..8, Some(BoxShape::Tab))],
    );
}

/// Tabs over `chrome.rs`, `panel.rs`, and a gzip `CHANGELOG.md.gz`, the last
/// one active, in a 60 by 8 harness whose workspace is `ws`.
fn tagged_tabs_harness() -> stoat::test_harness::TestHarness {
    let mut h = stoat::test_harness::TestHarness::with_size(60, 8);
    h.stoat.active_workspace_mut().name = "ws".to_string();
    let chrome = h.write_file("chrome.rs", "fn a() {}\n");
    h.open_file(&chrome);
    dispatch(&mut h.stoat, &stoat_action::NewTab);
    let panel = h.write_file("panel.rs", "fn b() {}\n");
    h.open_file(&panel);
    dispatch(&mut h.stoat, &stoat_action::NewTab);
    let changelog = PathBuf::from("/test/CHANGELOG.md.gz");
    h.fake_fs()
        .insert_file(&changelog, stoat::test_harness::gzip(b"# 1.0\n"));
    h.open_file(&changelog);
    h.settle();
    // A gzip file reads on the blocking pool, and the read installs here.
    buffer_lifecycle::install_pending_opens(&mut h.stoat);
    h
}

/// A conflicted file's tab leads with `!` in the conflict mark's color once
/// the tally lists the file. The conflict view's tab counts the files its
/// session resolves and keeps the mark. While the view is focused the bar
/// ends in a chip that names the operation and what is left of it, its text
/// a whole cell in from its edge.
#[test]
fn the_tab_bar_marks_a_conflicted_file_and_ends_in_the_conflict_chip() {
    let mut h = conflict_tabs_harness(&["f.txt"]);
    dispatch(
        &mut h.stoat,
        &OpenFile {
            path: PathBuf::from("/repo/f.txt"),
        },
    );
    h.settle_diff_jobs();
    let file_bar = tab_bar_row(&h.render_composited());

    dispatch(&mut h.stoat, &Conflict);
    h.settle();
    let buf = h.render_composited();
    let scene = stoatty_protocol::command::decode_stream(&h.stoat.apc_scene.all_bytes());
    let panels: Vec<_> = scene
        .iter()
        .filter_map(|command| match command {
            stoatty_protocol::command::Command::Panel(panel) if panel.top == 0 => {
                Some((panel.left, panel.width, panel.fill))
            },
            _ => None,
        })
        .collect();
    let chip_text = scene.iter().find_map(|command| match command {
        stoatty_protocol::command::Command::TextRun(run) if run.text.contains(" left \u{b7} ") => {
            Some((run.col, run.text.clone(), run.color))
        },
        _ => None,
    });

    let text = "conflict \u{b7} 1 conflict left \u{b7} 1 of 1 files";
    assert_eq!(
        (
            file_bar.chars().take(30).collect::<String>(),
            tab_bar_row(&buf).chars().take(30).collect::<String>(),
            buf[(6, 0)].fg,
            panels,
            chip_text,
        ),
        (
            format!("{:<30}", " ws   ! f.txt"),
            format!("{:<30}", " ws   ! conflicts \u{b7} 1 file"),
            ratatui::style::Color::Rgb(0xf5, 0x9a, 0xa0),
            vec![
                (5, 22, Some([0x24, 0x35, 0x52])),
                (56, 43, Some([0x33, 0x20, 0x1f])),
            ],
            Some((56 * 16 + 16, text.to_string(), [0xf2, 0x77, 0x7f])),
        ),
        "the active tab's chip in the accent wash, then the conflict chip one cell before \
         the bar's edge, its text a whole cell in",
    );
}

/// With a Job the conflict chip ends one blank cell before the tray. It
/// drops where the tabs leave it no room before the tray, though the bar
/// past the tray would hold it.
#[test]
fn the_conflict_chip_ends_left_of_the_tray() {
    let mut h = conflict_tabs_harness(&["f.txt"]);
    dispatch(&mut h.stoat, &stoat_action::NewTab);
    h.push_job("sleep 30");
    dispatch(&mut h.stoat, &stoat_action::PrevTab);
    dispatch(
        &mut h.stoat,
        &OpenFile {
            path: PathBuf::from("/repo/f.txt"),
        },
    );
    h.settle_diff_jobs();
    dispatch(&mut h.stoat, &Conflict);
    h.settle();
    let fills = [Some([0x33, 0x20, 0x1f]), Some([0x1e, 0x21, 0x27])];
    let boxes = |h: &mut stoat::test_harness::TestHarness| -> Vec<(u16, u16)> {
        h.render_composited();
        stoatty_protocol::command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .filter_map(|command| match command {
                stoatty_protocol::command::Command::Panel(panel)
                    if panel.top == 0 && fills.contains(&panel.fill) =>
                {
                    Some((panel.left, panel.width))
                },
                _ => None,
            })
            .collect()
    };

    let roomy = boxes(&mut h);
    let tabs_end = h.stoat.tab_bar_spans.last().map_or(0, |span| span.end / 16);
    h.stoat.active_workspace_mut().name = format!("ws{}", "x".repeat(usize::from(41 - tabs_end)));
    let crowded = boxes(&mut h);

    assert_eq!(
        (roomy, crowded),
        (vec![(38, 43), (82, 17)], vec![(82, 17)]),
        "the chip ends a cell before the tray, and drops once the tabs reach 41 columns",
    );
}

/// A tab whose file the conflict view applied reads U+2713 in the added
/// color. The view's own tab keeps its mark and counts the session's files,
/// and the chip counts the one file that still holds an open chunk.
#[test]
fn the_tab_bar_marks_an_applied_file_resolved() {
    let mut h = conflict_tabs_harness(&["a.txt", "b.txt"]);
    dispatch(
        &mut h.stoat,
        &OpenFile {
            path: PathBuf::from("/repo/a.txt"),
        },
    );
    dispatch(&mut h.stoat, &stoat_action::NewTab);
    h.settle_diff_jobs();
    dispatch(&mut h.stoat, &Conflict);
    h.settle();
    dispatch(&mut h.stoat, &stoat_action::ConflictPickOurs);
    dispatch(&mut h.stoat, &stoat_action::ConflictApply);
    h.settle();

    let buf = h.render_composited();
    let chip = stoatty_protocol::command::decode_stream(&h.stoat.apc_scene.all_bytes())
        .into_iter()
        .find_map(|command| match command {
            stoatty_protocol::command::Command::TextRun(run)
                if run.text.contains(" left \u{b7} ") =>
            {
                Some(run.text)
            },
            _ => None,
        });
    let added = h
        .stoat
        .theme
        .get(stoat::theme::scope::DIFF_ADDED)
        .fg
        .expect("the theme colors added text");
    assert_eq!(
        (
            tab_bar_row(&buf).chars().take(40).collect::<String>(),
            buf[(6, 0)].fg,
            chip,
        ),
        (
            format!("{:<40}", " ws   \u{2713} a.txt  ! conflicts \u{b7} 2 files"),
            added,
            Some("conflict \u{b7} 1 conflict left \u{b7} 1 of 2 files".to_string()),
        ),
    );
}

/// During a merge a changes view's tab bar holds a badge that names the
/// merge and counts the conflict blocks of the unmerged files and the
/// files. It stands one blank cell before the layout chips, which keep the
/// bar's end.
///
/// The badge's 34 characters and a cell a side take 36 cells, and the
/// layout chips the last 17, so the badge starts at cell 46. The block in
/// c.txt counts no conflict, since the tally leaves that file merged.
#[test]
fn a_changes_view_s_tab_bar_names_the_merge_left_of_its_layout_chips() {
    let row = merge_tab_row(100, true, true);
    assert_eq!(
        row.chars().skip(46).collect::<String>(),
        " merge main \u{b7} 3 conflicts \u{b7} 2 files   files  changes  ",
    );
}

/// A changes view shows no merge badge over conflicted files with no merge
/// in progress, nor during a merge that leaves no file unmerged. A bar too
/// narrow for the badge drops it and keeps its layout chips.
#[test]
fn a_changes_view_with_no_merge_shows_no_badge() {
    let tail = |row: String| row.chars().skip(46).collect::<String>();
    let blank = format!("{:37} files  changes  ", "");
    assert_eq!(
        [
            tail(merge_tab_row(100, false, true)),
            tail(merge_tab_row(100, true, false)),
            merge_tab_row(70, true, true),
        ],
        [
            blank.clone(),
            blank,
            format!(
                "{:<53} files  changes  ",
                " ws   \u{b1} changes \u{b7} 3 files"
            ),
        ],
    );
}

/// The tab bar of a changes view `width` cells wide, in cells, over a.txt,
/// whose text holds two conflict blocks, b.txt, which holds one, and c.txt,
/// which holds one too.
///
/// With `merge` the repository merges `main`, and with `conflicted` the
/// tally lists a.txt and b.txt as unmerged.
fn merge_tab_row(width: u16, merge: bool, conflicted: bool) -> String {
    let block = |n: u32| format!("<<<<<<< HEAD\nours {n}\n=======\ntheirs {n}\n>>>>>>> main\n");
    let mut h = stoat::test_harness::TestHarness::with_size(width, 30);
    h.stoat.settings.ui_tab_bar = Some(stoat_config::TabBarMode::Always);
    let ws = h.stoat.active_workspace_mut();
    ws.name = "ws".to_string();
    ws.git_root = PathBuf::from("/repo");
    {
        let mut repo = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
        let files = [
            ("a.txt", block(1) + &block(2)),
            ("b.txt", block(3)),
            ("c.txt", block(4)),
        ];
        for (file, text) in files {
            repo.modified(file, "x\n", &text);
            if conflicted && file != "c.txt" {
                repo.conflicted_file(file, Some("x\n"), Some("ours\n"), Some("theirs\n"));
            }
        }
        if merge {
            repo.merge_in_progress("f00dfeed", Some("main"), "Merge branch 'main'");
        }
    }
    h.open_file(Path::new("/repo/a.txt"));
    h.cover_with_diff_changes(false);
    h.settle();
    h.stoat.stoatty = false;
    tab_bar_row(&h.render_composited())
}

/// A 100 by 12 harness whose tab bar always shows, in workspace `ws` over
/// a fake repository at `/repo` where each of `files` is conflicted.
fn conflict_tabs_harness(files: &[&str]) -> stoat::test_harness::TestHarness {
    let mut h = stoat::test_harness::TestHarness::with_size(100, 12);
    h.stoat.settings.ui_tab_bar = Some(stoat_config::TabBarMode::Always);
    h.stoat.set_diff_warm_auto(true);
    let ws = h.stoat.active_workspace_mut();
    ws.name = "ws".to_string();
    ws.git_root = PathBuf::from("/repo");
    let mut repo = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
    for file in files {
        repo.conflicted_file(
            file,
            Some("a\nbase\n"),
            Some("a\nours\n"),
            Some("a\ntheirs\n"),
        );
    }
    h
}

/// The tab bar's row of `buf`, with the composite's blank bar cells read as
/// spaces.
fn tab_bar_row(buf: &Buffer) -> String {
    (0..buf.area.width)
        .map(|x| buf[(x, 0)].symbol())
        .collect::<String>()
        .replace('─', " ")
}

/// The status row names the workspace until a second tab shows the tab
/// bar, which then carries the name. The file name then meets the mode
/// chip across a gap cell.
#[test]
fn the_status_row_leaves_the_workspace_name_to_the_tab_bar() {
    let mut h = stoat::test_harness::TestHarness::with_size(60, 8);
    h.stoat.active_workspace_mut().name = "ws".to_string();
    // The left segments, which end where the bar's hairline run starts.
    let status_left = |h: &mut stoat::test_harness::TestHarness| {
        h.snapshot();
        let text = h.rendered_text();
        let row = text.lines().last().expect("the status row");
        row.split('─').next().unwrap_or_default().to_string()
    };

    let one_tab = status_left(&mut h);
    dispatch(&mut h.stoat, &stoat_action::NewTab);
    let two_tabs = status_left(&mut h);
    assert_eq!([one_tab, two_tabs], [" NOR  ws scratch ", " NOR  scratch "]);
}

/// A focused run pane's row leads with the run chip in either mode of its
/// input, counts the Cmds of its stack after its name, and names the
/// directory its commands run in at its right end, where an editor's row
/// shows the cursor. The repo segment stays off the
/// row, as the StartRun board draws the directory alone.
#[test]
fn a_focused_run_pane_s_row_reads_run_and_its_directory() {
    let mut h = stoat::test_harness::TestHarness::with_size(80, 12);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_fs().insert_file(Path::new("/repo/f.txt"), b"a\n");
    h.fake_git()
        .add_repo("/repo")
        .commit("a1000000", &[("f.txt", "a\n")])
        .head_file("f.txt", "a\n")
        .index_file("f.txt", "a\n")
        .set_head_branch("main");
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/f.txt"));
    h.settle_diff_jobs();
    h.open_run();
    h.stoat.home = Some(PathBuf::from("/home/u"));
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/home/u/code/stoat");
    let row = |h: &mut stoat::test_harness::TestHarness| {
        let row = bar_row(&h.render_composited()).replace('\u{2500}', " ");
        row.split_whitespace().collect::<Vec<_>>().join(" ")
    };

    let insert = row(&mut h);
    h.type_keys("escape");
    let normal = row(&mut h);

    assert_eq!(
        [insert, normal],
        ["RUN stoat run 0 Cmds ~/code/stoat"; 2].map(String::from),
    );
}

/// A focused Run pane's row counts the Cmds of its stack after its name,
/// in the singular for one. An unfocused Run pane's row names `run` alone,
/// even beside a focused Run pane, whose count is its own.
#[test]
fn a_run_pane_s_row_counts_its_cmds() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 12);
    h.stoat.home = Some(PathBuf::from("/home/u"));
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/home/u/code/stoat");
    h.type_action("SplitRight()");
    h.open_run();
    let run_row = |h: &mut stoat::test_harness::TestHarness| {
        let buf = h.render_composited();
        let ws = h.stoat.active_workspace();
        let area = ws
            .panes
            .split_panes()
            .filter(|(_, pane)| matches!(pane.view, View::Run(_)))
            .map(|(_, pane)| pane.area)
            .max_by_key(|area| area.x)
            .expect("a Run pane");
        // The rightmost pane's row runs on past its area to the window's edge.
        let row: String = (area.x..buf.area.right())
            .map(|x| buf[(x, area.bottom())].symbol())
            .collect();
        let row = row.replace('\u{2500}', " ");
        row.split_whitespace().collect::<Vec<_>>().join(" ")
    };

    let mut reads = vec![run_row(&mut h)];
    for line in ["l1", "l2"] {
        h.submit_run(line);
        reads.push(run_row(&mut h));
    }
    h.stoat.active_workspace_mut().panes.focus_next();
    h.open_run();
    reads.push(run_row(&mut h));

    assert_eq!(
        reads,
        [
            "RUN stoat run 0 Cmds ~/code/stoat",
            "RUN stoat run 1 Cmd ~/code/stoat",
            "RUN stoat run 2 Cmds ~/code/stoat",
            "run",
        ],
    );
}

/// Under `ui.tab_bar = auto` the bar shows while the workspace holds a Job,
/// whose Tray Chip it carries, and hides once the Job comes back.
#[test]
fn the_auto_tab_bar_shows_while_a_job_exists() {
    let mut h = stoat::test_harness::TestHarness::default();
    h.open_run();
    h.submit_run("sleep 30");
    let before = h.stoat.tab_bar_visible();
    h.type_keys("ctrl-j");
    let pushed = h.stoat.tab_bar_visible();
    h.type_keys("ctrl-j");

    assert_eq!(
        [before, pushed, h.stoat.tab_bar_visible()],
        [false, true, false]
    );
}

/// A focused Run pane's row counts the Cmds of its stack in Jobs after its
/// count of Cmds, while any is in Jobs.
///
/// The tab bar shows while the workspace holds a Job, so the row leaves out
/// the workspace name that the bar carries then.
#[test]
fn the_row_of_a_run_pane_counts_its_cmds_in_jobs() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 12);
    h.stoat.home = Some(PathBuf::from("/home/u"));
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/home/u/code/stoat");
    h.open_run();
    h.submit_run("sleep 30");
    let row = |h: &mut stoat::test_harness::TestHarness| {
        let row = bar_row(&h.render_composited()).replace('\u{2500}', " ");
        row.split_whitespace().collect::<Vec<_>>().join(" ")
    };

    let mut reads = vec![row(&mut h)];
    for _ in 0..2 {
        h.type_keys("ctrl-j");
        reads.push(row(&mut h));
    }
    assert_eq!(
        reads,
        [
            "RUN stoat run 1 Cmd ~/code/stoat",
            "RUN run 1 Cmd \u{b7} 1 in Jobs ~/code/stoat",
            "RUN stoat run 1 Cmd ~/code/stoat",
        ],
    );
}
