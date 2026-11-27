use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use std::{path::Path, sync::Arc, time::Duration};
use stoat::{
    action_handlers::{
        dispatch, focused_editor_mut, read_open_content, read_text_via_host, workspace, OpenContent,
    },
    app::{Stoat, UpdateEffect},
    buffer::OPS_COMPACT_THRESHOLD,
    host::FsHost,
    multi_buffer::MultiBuffer,
    pane::{DockPanel, DockSide, DockVisibility, FocusTarget, View},
    review_walk::{ReturnRef, ReviewWalk},
    selection::SelectionsCollection,
    test_harness::{draw_type_at_full_cell, editor, keys, stoat},
};
use stoat_action::{
    FocusPane, ForceQuit, ForceQuitAll, JumpBackward, MoveDown, MovePaneLeft, MovePaneNext,
    MovePaneRight, Quit, QuitAll, RenameWorkspace, SaveSelection, SetTheme, SplitNewRight,
    SplitRight,
};
use stoat_config::Settings;
use stoat_scheduler::TestScheduler;
use stoat_text::{Bias, BufferId, SelectionGoal};

fn stoat_with_config(user: Option<String>) -> Stoat {
    draw_type_at_full_cell();
    let scheduler = Arc::new(TestScheduler::new());
    let mut stoat = Stoat::new_with_user_config(
        scheduler.executor(),
        Settings::default(),
        std::path::PathBuf::new(),
        user,
        Vec::new(),
        None,
    );
    stoat.update(Event::Resize(80, 24));
    // Stands in for the ident handshake, as the shared harness does, so
    // these read as a real stoatty session rather than a foreign terminal.
    stoat.stoatty = true;
    stoat
}

/// A preview session bakes its syntax colors at build, so a theme switch
/// has to drop the cached ones and anything already building. Leaving them
/// would paint the old theme's colors under the new one.
#[test]
fn set_theme_drops_the_cached_commit_previews() {
    let mut h = Stoat::test();
    h.seed_linear_history("/repo", &[("a1b2c3d4", "one", &[("a.rs", "fn a() {}\n")])]);
    h.open_commits("/repo");

    {
        let ws = h.stoat.active_workspace_mut();
        let commits = ws.focused_commits_mut().expect("the commits view is open");
        let doc = Arc::new(stoat::review_session::DiffDocument::default());
        commits.preview_sessions.insert("a1b2c3d4".into(), doc);
        commits.requested_preview = Some("a1b2c3d4".into());
        assert!(
            commits.preview_sessions.mark_used("a1b2c3d4"),
            "the session is cached before the switch"
        );
    }

    dispatch(
        &mut h.stoat,
        &SetTheme {
            name: "default_dark".into(),
        },
    );

    let ws = h.stoat.active_workspace_mut();
    let commits = ws.focused_commits_mut().expect("still open");
    assert_eq!(
        (
            commits.preview_sessions.mark_used("a1b2c3d4"),
            commits.requested_preview.is_some(),
        ),
        (false, false),
        "the switch dropped the cached session and the pending request"
    );
}

#[test]
fn set_theme_switches_active_theme() {
    use ratatui::style::Color;
    use stoat::theme::scope::UI_TEXT;

    let mut stoat = stoat_with_config(Some("theme mine { ui.text.fg = \"#abcdef\"; }".to_string()));
    assert_eq!(stoat.theme.name, "ermine_night");

    let effect = dispatch(
        &mut stoat,
        &SetTheme {
            name: "mine".to_string(),
        },
    );
    assert_eq!(effect, UpdateEffect::Redraw);
    assert_eq!(stoat.theme.name, "mine");
    assert_eq!(
        stoat.theme.get(UI_TEXT).fg,
        Some(Color::Rgb(0xab, 0xcd, 0xef))
    );
}

/// The foreground a retained tree-sitter token channel currently resolves
/// `keyword`-scoped tokens to, and how many tokens carry that scope.
fn keyword_channel_color(
    h: &mut stoat::test_harness::TestHarness,
) -> (Option<ratatui::style::Color>, usize) {
    let keyword = stoat::display_map::syntax_theme::highlight_id_for_key("keyword")
        .expect("keyword is a theme key");
    let style_id = h
        .stoat
        .syntax_styles
        .id_for_highlight(keyword)
        .expect("keyword resolves");

    let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
    let snapshot = editor.display_map.snapshot();
    let channels = snapshot.semantic_token_highlights();
    let color = channels
        .values()
        .next()
        .map(|channel| channel.interner[style_id].foreground)
        .unwrap_or_default();
    let count = channels
        .values()
        .flat_map(|channel| channel.iter())
        .filter(|token| token.style == style_id)
        .count();
    (color, count)
}

/// A theme switch recolors tokens already on screen, with no edit to force
/// a reparse. This is the reported bug: a keyword kept the previous theme's
/// color, and further switches never reset it.
#[test]
fn set_theme_recolors_retained_tree_sitter_tokens() {
    let mut h = stoat::test_harness::TestHarness::with_size(24, 4);
    let path = std::path::PathBuf::from("/theme-swap/a.rs");
    h.fake_fs()
        .insert_files(std::iter::once((path.clone(), b"fn a() {}\n".as_slice())));
    h.stoat.active_workspace_mut().git_root = "/theme-swap".into();
    h.open_file(&path);
    h.snapshot();

    let (dark, count) = keyword_channel_color(&mut h);
    assert!(count > 0, "the parse produced keyword-scoped tokens");

    dispatch(
        &mut h.stoat,
        &SetTheme {
            name: "gruvbox-light".to_string(),
        },
    );
    let (light, after) = keyword_channel_color(&mut h);
    assert_eq!(after, count, "the same tokens are retained, not dropped");
    assert_ne!(
        light, dark,
        "the retained channel resolves keywords through the new theme"
    );

    dispatch(
        &mut h.stoat,
        &SetTheme {
            name: "ermine_night".to_string(),
        },
    );
    assert_eq!(
        keyword_channel_color(&mut h).0,
        dark,
        "switching back restores the original color rather than sticking"
    );
}

/// A minimap class is a theme-key index, so what a strip holds means the
/// same under any palette. Rebuilding it re-resolves every token on the run
/// loop and re-sends every line for a change of colors alone.
#[test]
fn set_theme_keeps_the_minimap_and_re_declares_its_palette() {
    use stoat::{apc_emit, minimap::emit::emit_minimap, test_fixture::drain_apc};
    use stoatty_protocol::command::Command;

    let mut h = stoat::test_harness::TestHarness::with_size(120, 24);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = std::path::PathBuf::from("/theme-minimap");
    let path = root.join("a.rs");
    h.fake_fs()
        .insert_file(&path, b"fn a() {}\nfn b() {}\nfn c() {}\n");
    h.stoat.active_workspace_mut().git_root = root;
    h.open_file(&path);
    h.settle();

    let _ = h.stoat.render();
    apc_emit::emit_apc_scene(&mut h.stoat);
    emit_minimap(&mut h.stoat);
    let built = drain_apc(&mut rx);
    assert!(
        built
            .iter()
            .any(|cmd| matches!(cmd, Command::MinimapLines(_))),
        "the strip is built before the switch, got {built:?}",
    );

    let stores: Vec<(u32, u64)> = h
        .stoat
        .minimap_content
        .values()
        .map(|content| (content.content_id(), content.synced_version()))
        .collect();
    assert!(!stores.is_empty(), "a strip is held for the open buffer");
    let palette = |cmds: &[Command]| {
        cmds.iter().find_map(|cmd| match cmd {
            Command::Minimap(strip) => Some(strip.palette.clone()),
            _ => None,
        })
    };
    let before = palette(&built).expect("the strip was declared");

    dispatch(
        &mut h.stoat,
        &SetTheme {
            name: "gruvbox-light".to_string(),
        },
    );
    let _ = h.stoat.render();
    apc_emit::emit_apc_scene(&mut h.stoat);
    emit_minimap(&mut h.stoat);
    let after = drain_apc(&mut rx);

    assert_eq!(
        h.stoat
            .minimap_content
            .values()
            .map(|content| (content.content_id(), content.synced_version()))
            .collect::<Vec<_>>(),
        stores,
        "every strip is kept where it was",
    );
    assert!(
        !after
            .iter()
            .any(|cmd| matches!(cmd, Command::MinimapLines(_))),
        "and no line is re-sent, got {after:?}",
    );
    assert!(
        palette(&after).is_some_and(|palette| palette != before),
        "while the strip is re-declared in the new theme's colors",
    );

    // A config reload replaces the theme by the other door and keeps the
    // same invariant.
    h.stoat
        .reload_user_config("theme mine { ui.text.fg = \"#123456\"; }");
    let _ = h.stoat.render();
    apc_emit::emit_apc_scene(&mut h.stoat);
    emit_minimap(&mut h.stoat);
    let reloaded = drain_apc(&mut rx);

    assert_eq!(
        h.stoat
            .minimap_content
            .values()
            .map(|content| (content.content_id(), content.synced_version()))
            .collect::<Vec<_>>(),
        stores,
        "a reload keeps them too",
    );
    assert!(
        !reloaded
            .iter()
            .any(|cmd| matches!(cmd, Command::MinimapLines(_))),
        "and re-sends no line, got {reloaded:?}",
    );
}

#[test]
fn gruvbox_light_is_selectable_as_a_builtin() {
    use ratatui::style::Color;
    use stoat::theme::scope::UI_BACKGROUND;

    let mut stoat = stoat_with_config(None);

    dispatch(
        &mut stoat,
        &SetTheme {
            name: "gruvbox-light".to_string(),
        },
    );

    assert_eq!(stoat.theme.name, "gruvbox-light");
    assert_eq!(
        stoat.theme.get(UI_BACKGROUND).bg,
        Some(Color::Rgb(0xfb, 0xf1, 0xc7)),
        "the builtin carries its light background through the import",
    );
}

#[test]
fn one_light_is_selectable_as_a_builtin() {
    use ratatui::style::Color;
    use stoat::theme::scope::UI_BACKGROUND;

    let mut stoat = stoat_with_config(None);

    dispatch(
        &mut stoat,
        &SetTheme {
            name: "one-light".to_string(),
        },
    );

    assert_eq!(stoat.theme.name, "one-light");
    assert_eq!(
        stoat.theme.get(UI_BACKGROUND).bg,
        Some(Color::Rgb(0xfa, 0xfa, 0xfa)),
        "the builtin carries its light background through the import",
    );
}

#[test]
fn a_theme_switch_pushes_its_colors_as_terminal_defaults() {
    let mut stoat = stoat_with_config(Some(
        "theme mine { ui.text.fg = \"#abcdef\"; ui.background.bg = \"#123456\"; }".to_string(),
    ));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    stoat.set_apc_tx(tx);

    dispatch(
        &mut stoat,
        &SetTheme {
            name: "mine".to_string(),
        },
    );

    let sent: Vec<u8> = std::iter::from_fn(|| rx.try_recv().ok())
        .flatten()
        .collect();
    assert_eq!(
        sent,
        b"\x1b]10;#abcdef\x1b\\\x1b]11;#123456\x1b\\".to_vec(),
        "the switch hands the terminal both defaults, foreground then background",
    );
}

#[test]
fn a_theme_without_a_background_pushes_only_the_foreground() {
    let mut stoat = stoat_with_config(Some("theme mine { ui.text.fg = \"#abcdef\"; }".to_string()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    stoat.set_apc_tx(tx);

    dispatch(
        &mut stoat,
        &SetTheme {
            name: "mine".to_string(),
        },
    );

    let sent: Vec<u8> = std::iter::from_fn(|| rx.try_recv().ok())
        .flatten()
        .collect();
    assert_eq!(
        sent,
        b"\x1b]10;#abcdef\x1b\\".to_vec(),
        "a color the theme does not define is left to the terminal",
    );
}

/// The pool content versions hash `theme_epoch`, so it has to move on a
/// real switch and hold still on a failed one. Otherwise a buffered page
/// either glides back in the old theme's colors or refills for nothing.
#[test]
fn set_theme_bumps_the_epoch_only_when_it_switches() {
    let mut stoat = stoat_with_config(Some("theme mine { ui.text.fg = \"#abcdef\"; }".to_string()));
    assert_eq!(stoat.theme_epoch, 0, "a fresh session starts at zero");

    dispatch(
        &mut stoat,
        &SetTheme {
            name: "mine".to_string(),
        },
    );
    assert_eq!(stoat.theme_epoch, 1, "a switch stales every buffered page");

    dispatch(
        &mut stoat,
        &SetTheme {
            name: "nonexistent".to_string(),
        },
    );
    assert_eq!(
        stoat.theme_epoch, 1,
        "a theme that failed to load repaints nothing, so nothing is staled",
    );
}

/// The paint generation moves when something a pane paints from outside its
/// display map moves, and holds otherwise.
///
/// The theme, the settings the renderer reads directly, and the search
/// query all reach the screen without passing through a display layer, so a
/// cache keyed on the display snapshot alone would hold a pane still
/// through a theme switch.
#[test]
fn a_theme_or_config_install_moves_the_paint_generation() {
    let mut stoat = stoat_with_config(Some("theme mine { ui.text.fg = \"#abcdef\"; }".to_string()));
    assert_eq!(stoat.paint_generation, 0, "a fresh session starts at zero");

    dispatch(
        &mut stoat,
        &SetTheme {
            name: "mine".to_string(),
        },
    );
    assert_eq!(stoat.paint_generation, 1, "a theme switch repaints panes");

    dispatch(
        &mut stoat,
        &SetTheme {
            name: "nonexistent".to_string(),
        },
    );
    assert_eq!(
        stoat.paint_generation, 1,
        "a theme that failed to load repaints nothing",
    );

    stoat.reload_user_config("theme mine { ui.text.fg = \"#123456\"; }");
    assert_eq!(
        stoat.paint_generation, 2,
        "a reload replaces the theme and the settings the renderer reads",
    );

    stoat.reload_user_config("this is not config {{{");
    assert_eq!(
        stoat.paint_generation, 2,
        "a parse failure keeps the current config, so it repaints nothing",
    );
}

#[test]
fn set_theme_unknown_keeps_theme_and_warns() {
    let mut stoat = stoat_with_config(None);
    assert_eq!(stoat.theme.name, "ermine_night");

    dispatch(
        &mut stoat,
        &SetTheme {
            name: "nonexistent".to_string(),
        },
    );
    assert_eq!(
        stoat.theme.name, "ermine_night",
        "an unknown theme keeps the current one",
    );
    assert!(
        stoat.pending_message.is_some(),
        "an unknown theme sets a status message",
    );
}

#[test]
fn dispatch_quit() {
    assert_eq!(dispatch(&mut stoat(), &Quit), UpdateEffect::Quit);
}

#[test]
fn dispatch_quit_with_splits_closes_pane() {
    let mut stoat = stoat();
    dispatch(&mut stoat, &SplitRight);
    assert_eq!(stoat.active_workspace().panes.pane_count(), 2);
    assert_eq!(dispatch(&mut stoat, &Quit), UpdateEffect::Redraw);
    assert_eq!(stoat.active_workspace().panes.pane_count(), 1);
}

#[test]
fn quit_on_the_last_pane_refuses_over_unsaved_edits() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "unsaved");
    assert_eq!(
        (
            dispatch(&mut stoat, &Quit),
            stoat.pending_message.as_deref()
        ),
        (
            UpdateEffect::Redraw,
            Some("1 unsaved buffer remaining: <scratch>")
        )
    );
}

#[test]
fn force_quit_exits_over_unsaved_edits() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "unsaved");
    assert_eq!(dispatch(&mut stoat, &ForceQuit), UpdateEffect::Quit);
}

#[test]
fn quit_closes_one_of_two_panes_over_unsaved_edits() {
    let mut stoat = stoat();
    dispatch(&mut stoat, &SplitRight);
    editor::seed_focused_buffer(&mut stoat, "unsaved");
    assert_eq!(
        (
            dispatch(&mut stoat, &Quit),
            stoat.active_workspace().panes.pane_count(),
            stoat.pending_message.as_deref()
        ),
        (UpdateEffect::Redraw, 1, None)
    );
}

fn focused_buffer(stoat: &mut Stoat) -> BufferId {
    focused_editor_mut(stoat).expect("an editor").buffer_id
}

/// Open `/quit/a.rs` and give it unsaved edits, then open the clean
/// `/quit/b.rs` over it in the same pane. Returns both buffer ids.
fn open_unsaved_then_clean(h: &mut stoat::test_harness::TestHarness) -> (BufferId, BufferId) {
    h.fake_fs().insert_file("/quit/a.rs", "a\n");
    h.fake_fs().insert_file("/quit/b.rs", "b\n");
    h.open_file(Path::new("/quit/a.rs"));
    h.seed_focused_buffer("edited\n");
    let unsaved = focused_buffer(&mut h.stoat);
    h.open_file(Path::new("/quit/b.rs"));
    (unsaved, focused_buffer(&mut h.stoat))
}

/// The refused quit brings the unsaved edits into view, and a backward
/// jump returns to the clean buffer, as Helix's switch records the jump.
#[test]
fn quit_on_a_clean_last_pane_shows_the_first_unsaved_buffer() {
    let mut h = Stoat::test();
    let (unsaved, clean) = open_unsaved_then_clean(&mut h);

    let effect = dispatch(&mut h.stoat, &Quit);
    let shown = focused_buffer(&mut h.stoat);
    let status = h.stoat.pending_message.clone();
    dispatch(&mut h.stoat, &JumpBackward);

    assert_eq!(
        (
            effect,
            shown,
            status.as_deref(),
            focused_buffer(&mut h.stoat)
        ),
        (
            UpdateEffect::Redraw,
            unsaved,
            Some("1 unsaved buffer remaining: a.rs"),
            clean
        )
    );
}

#[test]
fn quit_on_an_unsaved_last_pane_stays_on_it() {
    let mut h = Stoat::test();
    let (_, shown) = open_unsaved_then_clean(&mut h);
    h.seed_focused_buffer("edited\n");
    assert_eq!(
        (
            dispatch(&mut h.stoat, &Quit),
            focused_buffer(&mut h.stoat),
            h.stoat.pending_message.as_deref()
        ),
        (
            UpdateEffect::Redraw,
            shown,
            Some("2 unsaved buffers remaining: a.rs, b.rs")
        )
    );
}

/// The exit ends every workspace, so unsaved edits in another one refuse
/// it too. The pane stays, since those edits are not in this workspace.
#[test]
fn quit_on_the_last_pane_refuses_over_another_workspaces_unsaved_edits() {
    let mut h = Stoat::test();
    let home = h.stoat.active_workspace;
    let other = h.create_workspace();
    h.set_active_workspace(other);
    h.seed_focused_buffer("unsaved");
    h.set_active_workspace(home);
    let shown = focused_buffer(&mut h.stoat);
    assert_eq!(
        (
            dispatch(&mut h.stoat, &Quit),
            focused_buffer(&mut h.stoat),
            h.stoat.pending_message.as_deref()
        ),
        (
            UpdateEffect::Redraw,
            shown,
            Some("1 unsaved buffer remaining: <scratch>")
        )
    );
}

/// The space chord ends with a refused quit, as a Helix minor mode ends
/// after one key.
#[test]
fn a_refused_space_q_returns_to_normal_mode() {
    let mut h = Stoat::test();
    h.seed_focused_buffer("unsaved");
    h.type_keys("space q");
    assert_eq!(
        (h.stoat.focused_mode(), h.stoat.pending_message.as_deref()),
        ("normal", Some("1 unsaved buffer remaining: <scratch>"))
    );
}

#[test]
fn move_pane_right_swaps_content_and_follows_focus() {
    let mut stoat = stoat();
    dispatch(&mut stoat, &SplitRight);
    let right = stoat.active_workspace().panes.focus();
    let left = {
        let ids = stoat.active_workspace().panes.split_pane_ids();
        *ids.iter().find(|&&id| id != right).expect("a left pane")
    };
    stoat.active_workspace_mut().panes.set_focus(left);
    let moved = editor::editor_id_in_pane(&stoat, left);

    dispatch(&mut stoat, &MovePaneRight);

    assert_eq!(
        stoat.active_workspace().panes.focus(),
        right,
        "focus follows the content into the right slot"
    );
    assert_eq!(
        editor::editor_id_in_pane(&stoat, right),
        moved,
        "the content moved with focus"
    );
    assert_eq!(
        stoat.active_workspace().panes.pane_count(),
        2,
        "no pane was created or destroyed"
    );
}

#[test]
fn move_pane_against_the_edge_reports_and_changes_nothing() {
    let mut stoat = stoat();
    dispatch(&mut stoat, &SplitRight);
    let right = stoat.active_workspace().panes.focus();
    let editor = editor::editor_id_in_pane(&stoat, right);

    dispatch(&mut stoat, &MovePaneRight);

    assert_eq!(
        stoat.active_workspace().panes.focus(),
        right,
        "focus unchanged"
    );
    assert_eq!(
        editor::editor_id_in_pane(&stoat, right),
        editor,
        "content unchanged"
    );
    assert_eq!(
        stoat.pending_message.as_deref(),
        Some("no pane in that direction")
    );
}

#[test]
fn move_pane_next_wraps_from_the_last() {
    let mut stoat = stoat();
    dispatch(&mut stoat, &SplitRight);
    dispatch(&mut stoat, &SplitRight);

    let ids = stoat.active_workspace().panes.split_pane_ids();
    let (first, last) = (ids[0], *ids.last().expect("panes"));
    stoat.active_workspace_mut().panes.set_focus(last);
    let moved = editor::editor_id_in_pane(&stoat, last);

    dispatch(&mut stoat, &MovePaneNext);

    assert_eq!(
        stoat.active_workspace().panes.focus(),
        first,
        "next from the last wraps to the first"
    );
    assert_eq!(
        editor::editor_id_in_pane(&stoat, first),
        moved,
        "the content followed to the first slot"
    );
}

#[test]
fn move_with_a_dock_focused_reports() {
    let mut stoat = stoat();
    dispatch(&mut stoat, &SplitRight);

    let ws = stoat.active_workspace_mut();
    let dock = ws.docks.insert(DockPanel {
        view: View::Label("dock".into()),
        side: DockSide::Left,
        visibility: DockVisibility::Open { width: 20 },
        default_width: 20,
        area: Default::default(),
    });
    ws.focus = FocusTarget::Dock(dock);

    dispatch(&mut stoat, &MovePaneRight);

    assert_eq!(stoat.pending_message.as_deref(), Some("no pane to move"));
    assert!(
        matches!(stoat.active_workspace().focus, FocusTarget::Dock(_)),
        "focus stays on the dock"
    );
}

#[test]
fn move_from_a_widened_pane_unwidens_and_still_moves() {
    let mut stoat = stoat();
    dispatch(&mut stoat, &SplitRight);
    let right = stoat.active_workspace().panes.focus();
    let left = {
        let ids = stoat.active_workspace().panes.split_pane_ids();
        *ids.iter().find(|&&id| id != right).expect("a left pane")
    };

    assert!(
        stoat.active_workspace_mut().panes.widen(right),
        "the focused pane widens"
    );
    let moved = editor::editor_id_in_pane(&stoat, right);

    dispatch(&mut stoat, &MovePaneLeft);

    assert_eq!(
        stoat.active_workspace().panes.widened(),
        None,
        "the move collapsed the widen first"
    );
    assert_eq!(
        stoat.active_workspace().panes.focus(),
        left,
        "focus followed into the left slot"
    );
    assert_eq!(
        editor::editor_id_in_pane(&stoat, left),
        moved,
        "the content moved despite the widen"
    );
}

#[test]
fn split_inherits_source_cursor_and_scroll() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, &"x\n".repeat(100));

    // Drive real keypresses so the update path's view-follow scrolls the
    // source pane, matching a user who navigated before splitting.
    for _ in 0..50 {
        stoat.update(Event::Key(keys::key(KeyCode::Char('j'))));
    }

    let source_pane = stoat.active_workspace().panes.focus();
    let source_editor = editor::editor_id_in_pane(&stoat, source_pane);
    let cursor = editor::primary_head_offset(&mut stoat);
    let scroll = editor::editor_scroll_row(&stoat, source_editor);
    assert!(scroll > 0, "moving past one screen scrolls the source pane");

    dispatch(&mut stoat, &SplitRight);

    let new_pane = stoat.active_workspace().panes.focus();
    assert_ne!(new_pane, source_pane, "the split focuses a new pane");
    let new_editor = editor::editor_id_in_pane(&stoat, new_pane);
    assert_ne!(new_editor, source_editor, "the new pane has its own editor");
    assert_eq!(
        editor::primary_head_offset(&mut stoat),
        cursor,
        "the new pane inherits the source cursor",
    );
    assert_eq!(
        editor::editor_scroll_row(&stoat, new_editor),
        scroll,
        "the new pane inherits the source viewport",
    );

    stoat.active_workspace_mut().panes.set_focus(source_pane);
    assert_eq!(
        editor::primary_head_offset(&mut stoat),
        cursor,
        "the source cursor is unchanged",
    );
    assert_eq!(
        editor::editor_scroll_row(&stoat, source_editor),
        scroll,
        "the source viewport is unchanged",
    );
}

#[test]
fn dispatch_quit_all_exits_with_splits() {
    let mut stoat = stoat();
    dispatch(&mut stoat, &SplitRight);
    dispatch(&mut stoat, &SplitRight);
    assert_eq!(stoat.active_workspace().panes.pane_count(), 3);
    assert_eq!(dispatch(&mut stoat, &QuitAll), UpdateEffect::Quit);
}

#[test]
fn dispatch_focus_pane_by_number() {
    let mut stoat = stoat();
    dispatch(&mut stoat, &SplitRight);
    dispatch(&mut stoat, &SplitRight);
    let ids = stoat.active_workspace().panes.split_pane_ids();
    assert_eq!(ids.len(), 3);

    dispatch(&mut stoat, &FocusPane { index: 2 });
    assert_eq!(
        stoat.active_workspace().panes.focus(),
        ids[1],
        "FocusPane(2) focuses the second pane in layout order",
    );

    dispatch(&mut stoat, &FocusPane { index: 9 });
    assert_eq!(
        stoat.active_workspace().panes.focus(),
        ids[1],
        "an out-of-range index leaves focus unchanged",
    );
    assert_eq!(stoat.pending_message.as_deref(), Some("no pane 9"));
}

#[test]
fn dispatch_quit_all_with_dirty_buffer_opens_modal() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "unsaved");
    assert_eq!(dispatch(&mut stoat, &QuitAll), UpdateEffect::Redraw);
    assert!(stoat.quit_all_confirm.is_some());
}

#[test]
fn force_quit_all_exits_over_unsaved_edits_with_no_prompt() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "unsaved");
    assert_eq!(
        (
            dispatch(&mut stoat, &ForceQuitAll),
            stoat.quit_all_confirm.is_none()
        ),
        (UpdateEffect::Quit, true)
    );
}

#[test]
fn quit_all_confirm_y_quits() {
    let mut h = Stoat::test();
    h.seed_focused_buffer("unsaved");
    dispatch(&mut h.stoat, &QuitAll);
    assert!(h.stoat.quit_all_confirm.is_some());
    assert_eq!(
        h.stoat.update(Event::Key(keys::key(KeyCode::Char('y')))),
        UpdateEffect::Quit
    );
}

#[test]
fn quit_all_confirm_n_cancels() {
    let mut h = Stoat::test();
    h.seed_focused_buffer("unsaved");
    dispatch(&mut h.stoat, &QuitAll);
    assert_eq!(
        h.stoat.update(Event::Key(keys::key(KeyCode::Char('n')))),
        UpdateEffect::Redraw
    );
    assert!(h.stoat.quit_all_confirm.is_none());
}

#[test]
fn quit_all_confirm_esc_cancels() {
    let mut h = Stoat::test();
    h.seed_focused_buffer("unsaved");
    dispatch(&mut h.stoat, &QuitAll);
    assert_eq!(
        h.stoat.update(Event::Key(keys::key(KeyCode::Esc))),
        UpdateEffect::Redraw
    );
    assert!(h.stoat.quit_all_confirm.is_none());
}

#[test]
fn quit_all_confirm_ctrl_c_cancels() {
    let mut h = Stoat::test();
    h.seed_focused_buffer("unsaved");
    dispatch(&mut h.stoat, &QuitAll);
    let event = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(h.stoat.update(Event::Key(event)), UpdateEffect::Redraw);
    assert!(h.stoat.quit_all_confirm.is_none());
}

#[test]
fn split_new_right_uses_fresh_scratch_buffer() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "original");

    let original_pane = stoat.active_workspace().panes.focus();
    let original_buffer_id = match stoat.active_workspace().panes.pane(original_pane).view {
        View::Editor(eid) => stoat.active_workspace().editors[eid].buffer_id,
        _ => panic!("focused pane is not an editor"),
    };

    dispatch(&mut stoat, &SplitNewRight);
    assert_eq!(stoat.active_workspace().panes.pane_count(), 2);

    let ws = stoat.active_workspace();
    let new_pane = ws.panes.focus();
    assert_ne!(new_pane, original_pane);

    let new_editor_id = match ws.panes.pane(new_pane).view {
        View::Editor(eid) => eid,
        _ => panic!("new pane is not an editor"),
    };
    let new_buffer_id = ws.editors[new_editor_id].buffer_id;
    assert_ne!(new_buffer_id, original_buffer_id);

    let new_buffer = ws.buffers.get(new_buffer_id).expect("buffer exists");
    let guard = new_buffer.read().expect("buffer poisoned");
    assert_eq!(guard.snapshot.visible_text.to_string(), "");

    let original_buffer = ws.buffers.get(original_buffer_id).expect("buffer exists");
    let original_guard = original_buffer.read().expect("buffer poisoned");
    assert_eq!(original_guard.snapshot.visible_text.to_string(), "original");
}

#[test]
fn type_action_direct() {
    let mut h = Stoat::test();
    h.type_action("SetMode(space)");
    let last = h.frames().last().expect("no frames");
    assert_eq!(last.mode, "space");
}

#[test]
fn open_file_shows_in_focused_pane() {
    let mut h = Stoat::test();
    let path = h.write_file("test.txt", "hello world");

    h.open_file(&path);
    let frame = h.snapshot();
    assert_eq!(frame.pane_count, 1);
    h.assert_snapshot("open_file_shows_in_focused_pane");
}

#[test]
fn toggle_syntax_highlight_drops_coloring_then_restores() {
    let mut h = Stoat::test();
    let path = h.write_file("a.rs", "fn main() {\n    let x = 1;\n}\n");
    h.open_file(&path);

    let colored = h.stoat.render();

    dispatch(&mut h.stoat, &stoat_action::ToggleSyntaxHighlight);
    let plain = h.stoat.render();
    assert_ne!(
        colored, plain,
        "toggling syntax highlighting off recolors the render",
    );
    // The code is unchanged. The snapshot shows it in the default
    // foreground with search/diagnostic highlights still available.
    h.assert_snapshot("toggle_syntax_off");

    dispatch(&mut h.stoat, &stoat_action::ToggleSyntaxHighlight);
    let restored = h.stoat.render();
    assert_eq!(
        colored, restored,
        "toggling syntax highlighting back on restores the coloring",
    );
}

#[test]
fn toggle_minimap_flips_visibility() {
    let mut h = Stoat::test();
    assert!(h.stoat.minimap_enabled(), "enabled by default");
    dispatch(&mut h.stoat, &stoat_action::ToggleMinimap);
    assert!(!h.stoat.minimap_enabled(), "toggle hides the minimap");
    dispatch(&mut h.stoat, &stoat_action::ToggleMinimap);
    assert!(h.stoat.minimap_enabled(), "toggle shows it again");
}

#[test]
fn toggle_wrap_overrides_then_clears() {
    use stoat_config::WrapMode;
    let mut h = Stoat::test();
    let override_of = |h: &mut Stoat| focused_editor_mut(h).expect("focused editor").wrap_override;

    assert_eq!(override_of(&mut h.stoat), None, "no override by default");

    dispatch(&mut h.stoat, &stoat_action::ToggleWrap);
    assert_eq!(
        override_of(&mut h.stoat),
        Some(WrapMode::None),
        "the default editor_width mode toggles to none",
    );

    dispatch(&mut h.stoat, &stoat_action::ToggleWrap);
    assert_eq!(
        override_of(&mut h.stoat),
        None,
        "a second toggle clears the override",
    );

    h.stoat.settings.editor_wrap = Some(WrapMode::None);
    dispatch(&mut h.stoat, &stoat_action::ToggleWrap);
    assert_eq!(
        override_of(&mut h.stoat),
        Some(WrapMode::EditorWidth),
        "with wrap configured off, the toggle overrides to editor_width",
    );
}

#[test]
fn single_minimap_mode_resolves_and_survives_toggle() {
    use stoat_config::MinimapMode;
    let mut h = Stoat::test();
    h.stoat.settings.editor_minimap = Some(MinimapMode::Single);
    assert_eq!(h.stoat.minimap_mode(), MinimapMode::Single);
    assert!(h.stoat.minimap_enabled(), "single mode shows the minimap");
    dispatch(&mut h.stoat, &stoat_action::ToggleMinimap);
    assert!(!h.stoat.minimap_enabled(), "toggle hides it in single mode");
    dispatch(&mut h.stoat, &stoat_action::ToggleMinimap);
    assert!(h.stoat.minimap_enabled(), "toggle shows it again");
    assert_eq!(
        h.stoat.minimap_mode(),
        MinimapMode::Single,
        "and it returns to single, not per-pane"
    );
}

#[test]
fn unset_editor_minimap_defaults_to_single() {
    use stoat_config::MinimapMode;
    let mut h = Stoat::test();
    h.stoat.settings.editor_minimap = None;
    assert_eq!(
        h.stoat.minimap_mode(),
        MinimapMode::Single,
        "an unset editor.minimap defaults to the single strip"
    );
}

#[test]
fn toggle_key_hints_flips_visibility() {
    let mut h = Stoat::test();
    assert!(!h.stoat.key_hints_visible, "hidden by default");
    dispatch(&mut h.stoat, &stoat_action::ToggleKeyHints);
    assert!(h.stoat.key_hints_visible, "first toggle shows the hints");
    dispatch(&mut h.stoat, &stoat_action::ToggleKeyHints);
    assert!(!h.stoat.key_hints_visible, "second toggle hides the hints");
}

#[test]
fn dismiss_key_hints_hides_a_shown_box_and_no_ops_when_clear() {
    let mut h = Stoat::test();
    dispatch(&mut h.stoat, &stoat_action::ToggleKeyHints);
    assert!(h.stoat.key_hints_visible, "toggle shows the hints");

    let effect = dispatch(&mut h.stoat, &stoat_action::DismissKeyHints);
    assert!(!h.stoat.key_hints_visible, "dismiss hides the hints");
    assert_eq!(effect, UpdateEffect::Redraw, "hiding the box redraws");

    let effect = dispatch(&mut h.stoat, &stoat_action::DismissKeyHints);
    assert!(!h.stoat.key_hints_visible, "dismiss again stays hidden");
    assert_eq!(
        effect,
        UpdateEffect::None,
        "an already-hidden box schedules no frame"
    );
}

#[test]
fn toggle_key_hints_paints_the_overlay_in_normal_mode() {
    let mut h = stoat::test_harness::TestHarness::with_size(160, 60);

    assert!(
        !hint_box_visible(&mut h.stoat, "normal"),
        "normal mode shows no hint box by default",
    );
    dispatch(&mut h.stoat, &stoat_action::ToggleKeyHints);
    assert!(
        hint_box_visible(&mut h.stoat, "normal"),
        "toggling on paints the normal-mode hint box",
    );
    dispatch(&mut h.stoat, &stoat_action::DismissKeyHints);
    assert!(
        !hint_box_visible(&mut h.stoat, "normal"),
        "dismissing hides it again",
    );
}

#[test]
fn space_pane_display_chord_suppresses_the_hint_box() {
    let mut h = stoat::test_harness::TestHarness::with_size(160, 60);

    h.type_keys("space a");
    assert!(
        hint_box_visible(&mut h.stoat, "space_pane_nav"),
        "the parent chord mode auto-shows its hint box",
    );

    h.type_keys("e");
    assert!(
        !hint_box_visible(&mut h.stoat, "space_pane_display"),
        "the display chord suppresses the hint box so only the badges show",
    );
}

/// A chord auto-shows its hint box for the one press it takes. Pinning it
/// turns it into a run, so the box goes rather than cover the text until
/// Escape.
#[test]
fn a_pinned_chord_hides_the_hint_box() {
    let mut h = stoat::test_harness::TestHarness::with_size(160, 60);

    h.type_keys("space g");
    assert!(
        hint_box_visible(&mut h.stoat, "space_goto"),
        "the unpinned chord auto-shows its hint box",
    );

    h.type_keys("g");
    assert!(
        !hint_box_visible(&mut h.stoat, "space_goto"),
        "pinning the chord takes the box down",
    );
}

/// The reader who wants the box through the run says so once in the config.
#[test]
fn pin_hides_hints_off_keeps_the_box_through_a_pinned_chord() {
    let settings = Settings {
        ui_pin_hides_hints: Some(false),
        ..Settings::default()
    };
    let mut h = stoat::test_harness::TestHarness::new_with_settings(160, 60, settings);

    h.type_keys("space g g");
    assert!(
        hint_box_visible(&mut h.stoat, "space_goto"),
        "the setting keeps the box up while the chord is pinned",
    );
}

/// The toggle is a session flag the paint reads last, so it reaches a box
/// the pin took down.
#[test]
fn the_key_hints_toggle_shows_the_box_a_pin_hid() {
    let mut h = stoat::test_harness::TestHarness::with_size(160, 60);

    h.type_keys("space g g");
    assert!(
        !hint_box_visible(&mut h.stoat, "space_goto"),
        "the pinned chord starts with no box",
    );

    dispatch(&mut h.stoat, &stoat_action::ToggleKeyHints);
    assert!(
        hint_box_visible(&mut h.stoat, "space_goto"),
        "the toggle forces the box back on while the pin holds",
    );
}

/// Whether a rendered frame carries the ` {title} ` hint-box title. The rich
/// frame draws the box off-grid, so the scene is composited back before the
/// scan.
fn hint_box_visible(stoat: &mut Stoat, title: &str) -> bool {
    use stoat::test_harness::apc;
    use stoatty_protocol::command;

    let needle = format!(" {title} ");
    let mut buf = stoat.render();
    let cmds = command::decode_stream(&stoat.apc_scene.all_bytes());
    apc::composite_scene(&mut buf, &cmds);
    let area = buf.area;
    (area.y..area.y + area.height).any(|y| {
        let row: String = (area.x..area.x + area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect();
        row.contains(&needle)
    })
}

fn version_badge_label(stoat: &Stoat) -> Option<String> {
    let id = stoat
        .badges
        .find_by_source(stoat::badge::BadgeSource::Version)?;
    stoat.badges.get(id).map(|b| b.label.clone())
}

#[test]
fn show_version_badge_appends_stoatty_when_inside_it() {
    let mut h = Stoat::test();
    h.stoat.set_version_info("0.1.0 (aaa 2026-07-03)");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.fake_env()
        .set("STOATTY_VERSION", "0.2.0 (bbb 2026-07-03)");

    dispatch(&mut h.stoat, &stoat_action::ShowVersion);

    assert_eq!(
        version_badge_label(&h.stoat).as_deref(),
        Some("stoat 0.1.0 (aaa 2026-07-03) | stoatty 0.2.0 (bbb 2026-07-03)"),
    );
}

#[test]
fn show_version_badge_is_dismissed_on_the_next_key() {
    let mut h = Stoat::test();
    dispatch(&mut h.stoat, &stoat_action::ShowVersion);
    assert!(
        version_badge_label(&h.stoat).is_some(),
        "the badge is shown after ShowVersion",
    );

    h.type_keys("j");
    assert!(
        version_badge_label(&h.stoat).is_none(),
        "the next key press retires the version badge",
    );
}

#[test]
fn dispatch_show_cwd_reports_git_root() {
    let mut h = Stoat::test();
    h.stoat.active_workspace_mut().git_root = "/some/root".into();

    dispatch(&mut h.stoat, &stoat_action::ShowCwd);

    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("Current working directory is /some/root"),
    );
}

#[test]
fn open_file_replaces_focused_pane_does_not_split() {
    let mut h = Stoat::test();
    let a = h.write_file("a.txt", "file A");
    let b = h.write_file("b.txt", "file B");

    h.open_file(&a);
    h.open_file(&b);
    let frame = h.snapshot();
    assert_eq!(frame.pane_count, 1);
    h.assert_snapshot("open_file_replaces_focused_pane");
}

#[test]
fn split_then_open_creates_multi_pane_layout() {
    let mut h = Stoat::test();
    let a = h.write_file("a.txt", "AAA");
    let b = h.write_file("b.txt", "BBB");
    let c = h.write_file("c.txt", "CCC");

    h.open_file(&a);
    h.type_action("SplitRight()");
    h.open_file(&b);
    h.type_action("SplitRight()");
    h.open_file(&c);
    let frame = h.snapshot();
    assert_eq!(frame.pane_count, 3);
    h.assert_snapshot("split_then_open_three");
}

#[test]
fn open_missing_file_creates_empty_buffer() {
    let path = std::path::PathBuf::from("/test/does_not_exist.txt");

    let mut h = Stoat::test();
    h.open_file(&path);
    let frame = h.snapshot();
    assert_eq!(frame.pane_count, 1);
}

#[test]
fn open_file_via_fs_host_reads_from_fake_fs() {
    let mut h = Stoat::test();
    h.fake_fs
        .insert_file("/work/hello.txt", b"greetings from fake fs");
    h.stoat.open_file(Path::new("/work/hello.txt"));
    let ws = h.stoat.active_workspace();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let editor = ws.editors.get(editor_id).unwrap();
    let buffer = ws.buffers.get(editor.buffer_id).unwrap();
    let guard = buffer.read().unwrap();
    assert_eq!(
        guard.snapshot.visible_text.to_string(),
        "greetings from fake fs"
    );
}

#[test]
fn type_action_quit_from_space() {
    let mut h = Stoat::test();
    h.type_keys("space");
    h.type_action("Quit");
}

#[test]
#[should_panic(expected = "unreachable")]
fn type_action_unreachable_panics() {
    let mut h = Stoat::test();
    h.type_action("NonExistentAction");
}

#[test]
fn type_action_reaches_a_binding_gated_on_the_live_state() {
    let mut h = Stoat::test();
    h.open_side_by_side(("/pair/a.txt", "a\n"), ("/pair/b.txt", "b\n"));
    h.type_action("DiffPair()");
    assert_eq!(
        focused_editor_mut(&mut h.stoat).map(|editor| editor.diff_view),
        Some(true),
        "space h D reaches DiffPair only under the live pair value",
    );
}

#[test]
fn new_workspace_adds_fresh_workspace_and_switches() {
    let mut h = Stoat::test();
    let original = h.stoat.active_workspace;
    assert_eq!(h.stoat.workspaces.len(), 1);

    h.type_action("NewWorkspace()");

    assert_eq!(h.stoat.workspaces.len(), 2);
    assert_ne!(h.stoat.active_workspace, original);
    let new_ws = h.stoat.active_workspace();
    assert_eq!(new_ws.panes.pane_count(), 1);
    assert_eq!(new_ws.editors.len(), 1);
    assert!(new_ws.rebase.is_none());
}

#[test]
fn copy_workspace_duplicates_pane_layout() {
    let mut h = Stoat::test();
    h.type_action("SplitRight()");
    let before_pane_count = h.stoat.active_workspace().panes.pane_count();
    assert_eq!(before_pane_count, 2);

    h.type_action("CopyWorkspace()");

    assert_eq!(h.stoat.workspaces.len(), 2);
    let new_ws = h.stoat.active_workspace();
    assert_eq!(new_ws.panes.pane_count(), before_pane_count);
}

#[test]
fn copy_workspace_clones_buffer_contents() {
    let mut h = Stoat::test();
    h.fake_fs.insert_file("/work/note.txt", b"original text");
    h.stoat.open_file(Path::new("/work/note.txt"));

    h.type_action("CopyWorkspace()");

    let ws = h.stoat.active_workspace();
    let focused = ws.panes.focus();
    let editor_id = match &ws.panes.pane(focused).view {
        View::Editor(id) => *id,
        other => panic!("expected editor in focused pane, got {other:?}"),
    };
    let editor = ws.editors.get(editor_id).expect("editor missing");
    let buffer = ws.buffers.get(editor.buffer_id).expect("buffer missing");
    let guard = buffer.read().expect("buffer poisoned");
    assert_eq!(guard.snapshot.visible_text.to_string(), "original text");
}

/// A copy replays its buffers on the blocking pool rather than on the run
/// loop, so it waits under the restoring badge until the replay installs.
#[test]
fn copying_a_workspace_replays_on_the_pool() {
    let mut h = Stoat::test();
    h.fake_fs.insert_file("/work/note.txt", b"original text");
    h.stoat.open_file(Path::new("/work/note.txt"));
    h.stoat.active_workspace_mut().name = "source".to_owned();
    h.settle();

    let hops = h.blocking_calls();
    let name = h.stoat.active_workspace().name.clone();
    let state = |h: &stoat::test_harness::TestHarness| {
        let ws = h.stoat.active_workspace();
        (
            ws.badges
                .find_by_source(stoat::badge::BadgeSource::SessionRestore)
                .is_some(),
            h.blocking_calls() - hops,
            ws.name.clone(),
            ws.buffers.open_paths(),
        )
    };
    dispatch(&mut h.stoat, &stoat_action::CopyWorkspace);
    let pending = state(&h);
    h.settle();
    h.stoat.drive_background();

    assert_eq!(
        [pending, state(&h)],
        [
            (true, 0, name.clone(), Vec::new()),
            (
                false,
                1,
                name,
                vec![Path::new("/work/note.txt").to_path_buf()]
            ),
        ],
        "the copy waits under its badge and its source's name until one pool hop \
             replays its buffers",
    );
}

/// A compacted history drops the insertions that a live anchor names, so a
/// copy re-takes its anchors against the replayed seed. A cursor inside the
/// appended run stays where it was instead of falling to the file's top.
#[test]
fn a_copy_keeps_a_cursor_inside_a_compacted_buffers_appended_run() {
    let mut h = Stoat::test();
    h.fake_fs.insert_file("/work/long.txt", b"abcdefghij\n");
    h.stoat.open_file(Path::new("/work/long.txt"));
    let cursor = 11 + OPS_COMPACT_THRESHOLD / 2;

    let focused_buffer = |stoat: &mut Stoat| {
        let editor = focused_editor_mut(stoat).expect("an editor has the focus");
        let id = editor.buffer_id;
        stoat
            .active_workspace()
            .buffers
            .get(id)
            .expect("buffer open")
    };
    let buffer = focused_buffer(&mut h.stoat);
    {
        let mut guard = buffer.write().expect("buffer poisoned");
        for _ in 0..OPS_COMPACT_THRESHOLD {
            let end = guard.rope().len();
            guard.edit(end..end, "x");
        }
    }
    let multi = MultiBuffer::singleton(buffer).snapshot();
    let mut selections = SelectionsCollection::new();
    selections.insert_cursor(
        multi.anchor_at(cursor, Bias::Right),
        SelectionGoal::None,
        &multi,
    );
    focused_editor_mut(&mut h.stoat)
        .expect("an editor has the focus")
        .selections = selections;

    dispatch(&mut h.stoat, &stoat_action::CopyWorkspace);
    h.settle();
    h.stoat.drive_background();

    let head = focused_editor_mut(&mut h.stoat)
        .expect("the copy's editor has the focus")
        .selections
        .newest_anchor()
        .start;
    let landed = focused_buffer(&mut h.stoat)
        .read()
        .expect("buffer poisoned")
        .resolve_anchor(&head);
    assert_eq!(
        landed, cursor,
        "the copy's cursor stays inside the appended run"
    );
}

/// A copy leaves a blame checkout with its source, since the checkout's
/// HEAD and stash belong to the workspace that made them.
///
/// HEAD stands on the walk's commit, so a walk that reaches the copy passes
/// the check of HEAD and stays there.
#[test]
fn copy_workspace_leaves_a_blame_checkout_with_its_source() {
    let mut h = Stoat::test();
    h.fake_git().add_repo("/repo").set_head("c1");
    h.stoat.active_workspace_mut().review_walk = Some(ReviewWalk::blame_checkout(
        "c1",
        ReturnRef::Branch("main".into()),
        None,
    ));
    let source = h.stoat.active_workspace;

    h.type_action("CopyWorkspace()");
    h.settle();
    h.stoat.drive_background();
    h.settle();
    assert_eq!(
        (
            h.stoat.workspaces[source].review_walk.is_some(),
            h.stoat.active_workspace().review_walk.is_some(),
        ),
        (true, false),
    );
}

#[test]
fn copy_workspace_gets_distinct_uid() {
    let mut h = Stoat::test();
    let source_uid = h.stoat.active_workspace().uid;

    h.advance_clock(Duration::from_nanos(1));
    h.type_action("CopyWorkspace()");

    let copy_uid = h.stoat.active_workspace().uid;
    assert_ne!(
        source_uid, copy_uid,
        "copy must have its own uid so both workspaces can persist",
    );
}

#[test]
fn copy_workspace_gives_a_commits_pane_a_list_of_its_own() {
    let mut h = Stoat::test();
    h.seed_linear_history("/repo", &[("a1b2c3d4", "one", &[("a.rs", "1\n")])]);
    h.open_commits("/repo");
    let source = h.stoat.active_workspace;

    dispatch(&mut h.stoat, &stoat_action::CopyWorkspace);
    h.settle();
    h.stoat.drive_background();

    let copy = h.stoat.active_workspace();
    let live = match copy.panes.pane(copy.panes.focus()).view {
        View::Commits(id) => copy.commit_lists.contains_key(id),
        _ => false,
    };
    assert_eq!(
        (
            live,
            copy.commit_lists.len(),
            h.stoat.workspaces[source].commit_lists.len()
        ),
        (true, 1, 1),
        "the copy's commits pane names a list of its own, and the source keeps its list"
    );
}

#[test]
fn switch_workspace_opens_picker() {
    let mut h = Stoat::test();
    let first = h.stoat.active_workspace;
    h.type_action("NewWorkspace()");
    assert!(h.stoat.workspace_picker.is_none());

    h.type_action("SwitchWorkspace()");

    assert!(h.stoat.workspace_picker.is_some());
    let picker = h.stoat.workspace_picker.as_ref().unwrap();
    assert_eq!(
        picker.entries().iter().map(|e| e.id).collect::<Vec<_>>(),
        [Some(first)],
        "the untouched active workspace has no row"
    );
}

/// A launch that opens into its only workspace has nothing to pick from, so
/// the entry point the binary calls leaves the finder closed.
#[test]
fn the_bare_launch_entry_point_skips_a_lone_workspace() {
    let mut h = Stoat::test();
    assert!(h.stoat.workspace_picker.is_none());

    h.stoat.open_workspace_picker();

    assert!(
        h.stoat.workspace_picker.is_none(),
        "a lone row is the workspace the launch already sits in, so there is nothing to pick",
    );
}

/// A bare launch reaches the same finder the key does, through the entry
/// point the binary calls rather than through a dispatch.
#[test]
fn the_bare_launch_entry_point_opens_the_picker_with_a_second_workspace() {
    let mut h = Stoat::test();
    h.type_action("NewWorkspace()");

    h.stoat.open_workspace_picker();

    let entries = h
        .stoat
        .workspace_picker
        .as_ref()
        .map(|picker| picker.entries().len());
    assert_eq!(
        entries,
        Some(1),
        "the other workspace is the one row to pick"
    );
}

/// An explicit ask deserves an answer, so the dispatch path opens the finder
/// where the bare-launch wrapper skips it.
#[test]
fn switch_workspace_opens_the_picker_even_when_lone() {
    let mut h = Stoat::test();

    h.type_action("SwitchWorkspace()");

    let entries = h
        .stoat
        .workspace_picker
        .as_ref()
        .map(|picker| picker.entries().len());
    assert_eq!(
        entries,
        Some(1),
        "SwitchWorkspace answers the ask at one row"
    );
}

/// The picker opens on the session focused before this one, so Enter alone
/// returns there. The test clock moves only when told, so it advances
/// between the switches to give each workspace its own focus instant.
#[test]
fn picker_enter_switches_to_selected_workspace() {
    let mut h = Stoat::test();
    h.advance_clock(Duration::from_secs(1));
    h.type_action("NewWorkspace()");
    let second = h.stoat.active_workspace;
    h.advance_clock(Duration::from_secs(1));
    h.type_action("NewWorkspace()");
    assert_eq!(h.stoat.workspaces.len(), 3);

    h.type_action("SwitchWorkspace()");
    h.type_keys("enter");

    assert_eq!(
        (h.stoat.workspace_picker.is_none(), h.stoat.active_workspace),
        (true, second),
        "Enter closes the picker on the previous session"
    );
}

#[test]
fn picker_escape_closes_without_switching() {
    let mut h = Stoat::test();
    h.type_action("NewWorkspace()");
    let before = h.stoat.active_workspace;

    h.type_action("SwitchWorkspace()");
    h.type_keys("escape");

    assert!(h.stoat.workspace_picker.is_none());
    assert_eq!(h.stoat.active_workspace, before);
}

#[test]
fn close_workspace_refuses_when_only_one_remains() {
    let mut h = Stoat::test();
    let only = h.stoat.active_workspace;

    dispatch(&mut h.stoat, &stoat_action::CloseWorkspace);

    assert_eq!(h.stoat.workspaces.len(), 1);
    assert_eq!(h.stoat.active_workspace, only);
}

#[test]
fn snapshot_workspace_picker_listing() {
    let mut h = Stoat::test();
    h.type_action("NewWorkspace()");
    // NewWorkspace builds a Workspace via the production path, which
    // generates a random uid-derived name. Clear it before opening
    // the picker so the rendered snapshot is stable across runs.
    for (_, ws) in h.stoat.workspaces.iter_mut() {
        ws.name = String::new();
    }
    h.type_action("SwitchWorkspace()");
    h.assert_snapshot("workspace_picker_listing");
}

/// The picker's hints used to be a hand-written list that had already lost
/// track of Tab. Naming that binding pins the box to the keymap, so a
/// future hand-written list cannot quietly reintroduce the drift. The
/// picker shows its box on the hints toggle alone.
#[test]
fn the_picker_hints_list_every_binding_the_keymap_holds() {
    let mut h = stoat::test_harness::TestHarness::with_size(160, 40);
    h.stoat.key_hints_visible = true;
    h.type_action("SwitchWorkspace()");
    h.snapshot();

    let buf = h.rendered_buffer();
    let painted: String = (buf.area.y..buf.area.y + buf.area.height)
        .map(|row| {
            (buf.area.x..buf.area.x + buf.area.width)
                .map(|col| buf[(col, row)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    for expected in [
        "complete from the picker",
        "page the picker down",
        "rename the picker row's session",
        "toggle the workspace picker scope",
        "cancel prompt input",
    ] {
        assert!(
            painted.contains(expected),
            "the hints box lists {expected:?}, which config binds:\n{painted}"
        );
    }
}

#[test]
fn typing_in_the_picker_filters_the_list() {
    let mut h = Stoat::test();
    h.type_action("NewWorkspace()");
    h.type_action("NewWorkspace()");
    let active = h.stoat.active_workspace;
    let ids: Vec<_> = h
        .stoat
        .workspaces
        .keys()
        .filter(|&id| id != active)
        .collect();
    h.stoat.workspaces[ids[0]].name = "alpha".into();
    h.stoat.workspaces[ids[1]].name = "beta".into();

    h.type_action("SwitchWorkspace()");
    assert_eq!(
        h.stoat.workspace_picker.as_ref().unwrap().filtered().len(),
        2,
        "both other workspaces list before filtering"
    );

    h.type_text("alph");

    let picker = h.stoat.workspace_picker.as_ref().unwrap();
    assert_eq!(picker.filtered().len(), 1, "typing narrows to the match");
    assert_eq!(
        picker.selected_entry().map(|e| e.basename.as_str()),
        Some("alpha")
    );
}

#[test]
fn fresh_workspace_is_fresh() {
    let h = Stoat::test();
    assert!(h.stoat.active_workspace().is_fresh());
}

#[test]
fn typing_in_scratch_breaks_freshness() {
    let mut h = Stoat::test();
    h.edit_focused(0..0, "x");
    assert!(!h.stoat.active_workspace().is_fresh());
}

#[test]
fn opening_file_breaks_freshness() {
    let mut h = Stoat::test();
    h.fake_fs.insert_file("/work/note.txt", b"hello");
    h.stoat.open_file(Path::new("/work/note.txt"));
    assert!(!h.stoat.active_workspace().is_fresh());
}

#[test]
fn splitting_pane_breaks_freshness() {
    let mut h = Stoat::test();
    h.type_action("SplitRight()");
    assert!(!h.stoat.active_workspace().is_fresh());
}

#[test]
fn opening_a_tab_breaks_freshness() {
    let mut h = Stoat::test();
    h.type_action("NewTab()");
    assert!(!h.stoat.active_workspace().is_fresh());
}

#[test]
fn a_file_buffer_behind_a_closed_pane_breaks_freshness() {
    let mut h = Stoat::test();
    h.type_action("SplitRight()");
    h.fake_fs.insert_file("/work/note.txt", b"hello");
    h.stoat.open_file(Path::new("/work/note.txt"));
    h.type_action("ClosePane()");
    assert!(!h.stoat.active_workspace().is_fresh());
}

#[test]
fn an_open_workspace_picker_keeps_a_workspace_fresh() {
    let mut h = Stoat::test();
    workspace::open_workspace_picker(&mut h.stoat);
    assert!(h.stoat.workspace_picker.is_some(), "the picker is open");
    assert!(h.stoat.active_workspace().is_fresh());
}

#[test]
fn typing_in_a_picker_filter_keeps_a_workspace_fresh() {
    let mut h = Stoat::test();
    workspace::open_workspace_picker(&mut h.stoat);
    h.type_text("abc");
    assert!(h.stoat.workspace_picker.is_some(), "the picker is open");
    assert!(h.stoat.active_workspace().is_fresh());
}

#[test]
fn an_open_palette_keeps_a_workspace_fresh() {
    let mut h = Stoat::test();
    h.type_keys(":");
    assert!(h.stoat.command_palette.is_some(), "the palette is open");
    assert!(h.stoat.active_workspace().is_fresh());
}

#[test]
fn a_file_open_behind_a_picker_breaks_freshness() {
    let mut h = Stoat::test();
    h.fake_fs.insert_file("/work/note.txt", b"hello");
    h.stoat.open_file(Path::new("/work/note.txt"));
    workspace::open_workspace_picker(&mut h.stoat);
    assert!(!h.stoat.active_workspace().is_fresh());
}

#[test]
fn close_workspace_switches_to_sibling() {
    let mut h = Stoat::test();
    let first = h.stoat.active_workspace;
    h.type_action("NewWorkspace()");
    let second = h.stoat.active_workspace;
    assert_ne!(first, second);

    h.type_action("CloseWorkspace()");

    assert_eq!(h.stoat.workspaces.len(), 1);
    assert_eq!(h.stoat.active_workspace, first);
    assert!(h.stoat.workspaces.get(second).is_none());
}

#[test]
fn rename_workspace_sets_active_workspace_name() {
    let mut stoat = stoat();
    dispatch(
        &mut stoat,
        &RenameWorkspace {
            name: "alpha".to_string(),
        },
    );
    assert_eq!(stoat.active_workspace().name, "alpha");
}

#[test]
fn rename_workspace_with_empty_name_clears_back_to_fallback() {
    let mut stoat = stoat();
    dispatch(
        &mut stoat,
        &RenameWorkspace {
            name: "alpha".to_string(),
        },
    );
    assert_eq!(stoat.active_workspace().name, "alpha");
    dispatch(
        &mut stoat,
        &RenameWorkspace {
            name: String::new(),
        },
    );
    assert_eq!(stoat.active_workspace().name, "");
}

#[test]
fn last_picker_action_records_command_palette() {
    let mut stoat = stoat();
    dispatch(&mut stoat, &stoat_action::OpenCommandPalette);
    assert_eq!(stoat.last_picker_action, Some("OpenCommandPalette"));
}

/// The palette owns an editor of its own, so a close leaves the pane
/// editor in the mode it held when the palette opened over it.
#[test]
fn open_command_palette_from_insert_mode_leaves_insert_behind_it() {
    let mut stoat = stoat();
    stoat.set_focused_mode("insert".into());

    dispatch(&mut stoat, &stoat_action::OpenCommandPalette);
    dispatch(&mut stoat, &stoat_action::CancelPromptInput);

    assert_eq!(stoat.focused_mode(), "insert");
}

#[test]
fn last_picker_recall_no_op_with_no_history() {
    let mut stoat = stoat();
    assert_eq!(
        dispatch(&mut stoat, &stoat_action::OpenLastPicker),
        UpdateEffect::None
    );
    assert!(stoat.command_palette.is_none());
    assert!(stoat.jumplist_picker.is_none());
    assert!(stoat.diagnostics_picker.is_none());
}

#[test]
fn last_picker_recall_reopens_jumplist_after_close() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "alpha\nbeta\ngamma\n");
    dispatch(&mut stoat, &SaveSelection);
    dispatch(&mut stoat, &MoveDown);
    dispatch(&mut stoat, &SaveSelection);
    dispatch(&mut stoat, &stoat_action::OpenJumplistPicker);
    assert_eq!(
        stoat.last_picker_action,
        Some("OpenJumplistPicker"),
        "open should record the recall target"
    );
    stoat.jumplist_picker = None;
    assert_eq!(
        dispatch(&mut stoat, &stoat_action::OpenLastPicker),
        UpdateEffect::Redraw
    );
    assert!(stoat.jumplist_picker.is_some(), "recall should reopen");
}

/// The recall chord hands the space leader's mode back, and the order
/// decides which editor takes it. After the re-open, focus is the recalled
/// picker's own input, so a switch there drops it out of insert.
///
/// A picker that reopens but takes no keys reads as a broken chord. It sits
/// on screen, and nothing the user types reaches it.
#[test]
fn the_recall_chord_reopens_a_picker_that_takes_keys() {
    let mut h = stoat::test_harness::TestHarness::with_size(80, 20);
    dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
    h.type_keys("escape");

    h.type_keys("space '");
    h.type_text("th");

    let palette = h
        .stoat
        .command_palette
        .as_ref()
        .expect("the chord reopens the palette");
    assert_eq!(
        palette.input.text(h.stoat.active_workspace()),
        "th",
        "the recalled palette takes the keys typed into it",
    );
    assert_eq!(h.stoat.focused_mode(), "insert", "its input stays live");
}

/// The text and compressed flag a read of `path` hands back, or the kind
/// of its error.
fn read_text(fs: &stoat::host::FakeFs, path: &str) -> Result<(String, bool), std::io::ErrorKind> {
    match read_open_content(fs, Path::new(path)) {
        Ok(OpenContent::Text { text, compressed }) => Ok((text, compressed)),
        Ok(_) => panic!("{path} read as something other than text"),
        Err(e) => Err(e.kind()),
    }
}

#[test]
fn a_gzip_file_opens_as_its_decompressed_text() {
    let fs = stoat::host::FakeFs::new();
    let bytes = stoat::test_harness::gzip(b"fn main() {}\n");
    fs.insert_file("/w/main.rs.gz", &bytes);

    let read = read_text(&fs, "/w/main.rs.gz");
    let mut on_disk = Vec::new();
    fs.read(Path::new("/w/main.rs.gz"), &mut on_disk)
        .expect("read");

    assert_eq!(
        (read, on_disk),
        (Ok(("fn main() {}\n".to_string(), true)), bytes),
        "the open decompresses in memory, and the file keeps its gzip bytes",
    );
}

#[test]
fn a_truncated_gzip_stream_fails_as_invalid_data() {
    let fs = stoat::host::FakeFs::new();
    let bytes = stoat::test_harness::gzip(b"fn main() {}\n");
    fs.insert_file("/w/cut.gz", &bytes[..bytes.len() / 2]);

    assert_eq!(
        read_text(&fs, "/w/cut.gz"),
        Err(std::io::ErrorKind::InvalidData)
    );
}

#[test]
fn read_text_via_host_decompresses_gzip() {
    let fs = stoat::host::FakeFs::new();
    fs.insert_file("/w/a.gz", stoat::test_harness::gzip(b"hello\n"));
    fs.insert_file("/w/b.txt", b"plain\n");
    let read = |path: &str| read_text_via_host(&fs, Path::new(path)).ok();

    assert_eq!(
        [read("/w/a.gz"), read("/w/b.txt")],
        [Some("hello\n".to_string()), Some("plain\n".to_string())]
    );
}
