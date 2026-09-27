use super::*;
use crate::{
    action_handlers::movement,
    badge::{Anchor as BadgeAnchor, Badge, BadgeSource, BadgeState},
    debounce,
};

/// The rich review gutter engages only when every color resolves to RGB, so
/// tests need a hex theme. The default theme uses named colors.
fn rgb_review_theme() -> crate::theme::Theme {
    let src = r##"theme rgbtest {
            diff.context.fg = "#808080";
            diff.added.fg = "#00ff00";
            diff.deleted.fg = "#ff0000";
            diff.current_hunk.fg = "#00ffff";
            ui.text.muted.fg = "#606060";
            ui.background.bg = "#282c34";
        }"##;
    let (config, _) = stoat_config::parse(src);
    crate::theme::Theme::from_config(&config.expect("theme config parses"), "rgbtest")
        .expect("rgb theme builds")
}

fn rgb_diagnostic_theme() -> crate::theme::Theme {
    let src = r##"theme rgbdiag {
            ui.diagnostic.error.fg = "#ff0000";
            ui.diagnostic.warning.fg = "#ffff00";
            ui.diagnostic.info.fg = "#00ffff";
            ui.diagnostic.hint.fg = "#808080";
            ui.text.muted.fg = "#606060";
            ui.background.bg = "#282c34";
        }"##;
    let (config, _) = stoat_config::parse(src);
    crate::theme::Theme::from_config(&config.expect("theme config parses"), "rgbdiag")
        .expect("rgb theme builds")
}

/// modal_frame's rich arm engages only when the border fg and the mask bg
/// both resolve to RGB, so the modal APC tests need a hex theme. The default
/// theme uses named colors and would fall back to glyphs.
fn rgb_modal_theme() -> crate::theme::Theme {
    let src = r##"theme rgbmodal {
            ui.modal.help.fg = "#8899aa";
            ui.modal.hints.fg = "#8899aa";
            ui.text.fg = "#c8ccd4";
            ui.text.muted.fg = "#606060";
            ui.border.inactive.fg = "#606060";
            ui.key_label.fg = "#d19a66";
            ui.background.bg = "#282c34";
        }"##;
    let (config, _) = stoat_config::parse(src);
    crate::theme::Theme::from_config(&config.expect("theme config parses"), "rgbmodal")
        .expect("rgb theme builds")
}

/// A hex theme for the pane-divider APC test, so the border colors resolve
/// to RGB and the stoatty arm emits bars instead of glyphs.
fn rgb_border_theme() -> crate::theme::Theme {
    let src = r##"theme rgbborder {
            ui.border.focused.fg = "#aabbcc";
            ui.border.inactive.fg = "#556677";
        }"##;
    let (config, _) = stoat_config::parse(src);
    crate::theme::Theme::from_config(&config.expect("theme config parses"), "rgbborder")
        .expect("rgb theme builds")
}

use crate::{
    action_handlers, app,
    term_session::TermSession,
    test_fixture::{
        drain_apc, finder_layout, help_layout, open_with_minimap_strip, palette_sizing,
    },
};
use std::path::PathBuf;
use stoat_action::{Conflict, OpenFile};
use stoat_config::MinimapMode;
use stoatty_protocol::{command, window_ipc::WindowIpcEvent};

/// A fill frame carries raw ANSI between its APC markers, so a terminal that
/// drops the markers prints the payload over the screen. Nothing may reach
/// the wire before a listener is confirmed.
#[test]
fn a_frame_emits_nothing_until_a_stoatty_is_confirmed() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.stoatty = false;

    let root = PathBuf::from("/gate");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    emit_smooth_scroll(&mut h.stoat);
    emit_apc_scene(&mut h.stoat);
    assert_eq!(
        drain_apc(&mut rx),
        Vec::new(),
        "no pool is declared, so nothing is left needing a drop either"
    );

    h.stoat.stoatty = true;
    emit_smooth_scroll(&mut h.stoat);
    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.iter().any(|cmd| matches!(cmd, Command::PoolRegion(_))),
        "and the same frame declares the editor pool once one answers, got {cmds:?}"
    );
}

#[test]
fn detach_emits_window_open_and_reattach_emits_window_close() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.window_ipc_connected = true;

    let path = PathBuf::from("/w/a.txt");
    h.fake_fs().insert_file(&path, b"hello\n");
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    h.resize(80, 24);
    h.type_action("SplitRight()");
    h.settle();

    h.type_action("DetachPane()");
    emit_windows(&mut h.stoat);
    let opened = drain_apc(&mut rx);
    assert!(
        opened.iter().any(|c| matches!(c, Command::WindowOpen(_))),
        "detach opens a window, got {opened:?}"
    );

    emit_windows(&mut h.stoat);
    assert!(
        drain_apc(&mut rx).is_empty(),
        "an unchanged frame emits no window commands"
    );

    h.type_action("ReattachPane()");
    emit_windows(&mut h.stoat);
    let closed = drain_apc(&mut rx);
    assert!(
        closed.iter().any(|c| matches!(c, Command::WindowClose(_))),
        "reattach closes the window, got {closed:?}"
    );
}

#[test]
fn editor_pool_pane_region_is_content_rect() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/pool");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let pane_id = h.stoat.active_workspace().panes.focus();
    h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(2, 1, 76, 23);

    let panes = editor_pool_panes(&h.stoat);
    assert_eq!(panes.len(), 1, "one editor pane is pooled");
    let (_, _, region) = panes[0];
    // Content rect is the pane area minus its one-row status bar.
    assert_eq!(
        (region.top, region.left, region.width, region.height),
        (1, 2, 76, 22)
    );
}

#[test]
fn editor_pool_pane_region_excludes_the_minimap_strip() {
    let mut h = Stoat::test();
    let editor_id = open_with_minimap_strip(&mut h);

    let pane_id = h.stoat.active_workspace().panes.focus();
    h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(2, 1, 76, 23);

    let (_, _, region) = editor_pool_panes(&h.stoat)[0];
    // The 8-column strip is reserved so a glide's pool composite cannot paint
    // over it, leaving content width 76 minus the strip's 8 columns.
    assert_eq!(
        (region.top, region.left, region.width, region.height),
        (1, 2, 68, 22)
    );

    h.stoat.active_workspace_mut().editors[editor_id].minimap_rect = None;
    let (_, _, region) = editor_pool_panes(&h.stoat)[0];
    assert_eq!(region.width, 76, "no strip restores the full content width");
}

#[test]
fn no_pool_pane_when_pane_is_not_an_editor() {
    let mut h = Stoat::test();
    let pane_id = h.stoat.active_workspace().panes.focus();
    h.stoat.active_workspace_mut().panes.pane_mut(pane_id).view = View::Label("scratch".into());
    assert!(editor_pool_panes(&h.stoat).is_empty());
}

#[test]
fn emit_smooth_scroll_retires_pools_in_overlay_mode() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/pool");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    emit_smooth_scroll(&mut h.stoat);
    let first = drain_apc(&mut rx);
    assert!(
        first
            .iter()
            .any(|cmd| matches!(cmd, Command::PoolRegion(_))),
        "first emit declares the editor pool, got {first:?}"
    );

    // Entering a full-screen overlay screen retires the editor pool.
    h.stoat.active_workspace_mut().rebase = Some(crate::rebase::RebaseState::new(
        PathBuf::from("/pool"),
        "onto".into(),
        vec![],
    ));
    emit_smooth_scroll(&mut h.stoat);
    let cmds = drain_apc(&mut rx);
    assert!(
        !cmds.is_empty() && cmds.iter().all(|cmd| matches!(cmd, Command::PoolDrop(_))),
        "overlay mode only drops pools, got {cmds:?}"
    );
}

#[test]
fn emit_smooth_scroll_pools_the_hover_and_retires_it_on_close() {
    use crate::render::hover::HoverPopup;
    use ratatui::style::Style;
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/pool");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    h.stoat.render();

    let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
    h.stoat.pending_hover = Some(HoverPopup::new(
        vec![vec![("hovered".to_string(), Style::default())]],
        0,
        editor_id,
    ));
    emit_smooth_scroll(&mut h.stoat);
    let opened = drain_apc(&mut rx);
    assert!(
        opened.iter().any(|cmd| matches!(
            cmd,
            Command::PoolRegion(r) if r.pool == crate::smooth_scroll::non_pane_pool::HOVER
        )),
        "an open hover emits its pool region, got {opened:?}"
    );

    h.stoat.pending_hover = None;
    emit_smooth_scroll(&mut h.stoat);
    let closed = drain_apc(&mut rx);
    assert!(
        closed.iter().any(|cmd| matches!(
            cmd,
            Command::PoolDrop(d) if d.pool == crate::smooth_scroll::non_pane_pool::HOVER
        )),
        "closing the hover retires its pool, got {closed:?}"
    );
}

#[test]
fn a_live_hover_selection_retires_the_pool() {
    use crate::render::hover::{HoverPopup, HoverSelection};
    use ratatui::style::Style;
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    let root = PathBuf::from("/pool");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    h.stoat.render();

    let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
    h.stoat.pending_hover = Some(HoverPopup::new(
        vec![vec![("hovered".to_string(), Style::default())]],
        0,
        editor_id,
    ));
    emit_smooth_scroll(&mut h.stoat);
    let opened = drain_apc(&mut rx);
    assert!(
        opened.iter().any(|cmd| matches!(
            cmd,
            Command::PoolRegion(r) if r.pool == crate::smooth_scroll::non_pane_pool::HOVER
        )),
        "an unselected hover pools its body, got {opened:?}",
    );

    if let Some(popup) = h.stoat.pending_hover.as_mut() {
        popup.selection = Some(HoverSelection {
            anchor: (0, 0),
            head: (0, 3),
            dragging: false,
        });
    }
    emit_smooth_scroll(&mut h.stoat);
    let dropped = drain_apc(&mut rx);
    assert!(
        dropped.iter().any(|cmd| matches!(
            cmd,
            Command::PoolDrop(d) if d.pool == crate::smooth_scroll::non_pane_pool::HOVER
        )),
        "a live selection retires the pool so the live frame owns it, got {dropped:?}",
    );
}

/// A hover harness with the popup open and rendered once, so the stamped
/// rects and anchor are what a live frame left behind.
fn hover_harness() -> (
    crate::test_harness::TestHarness,
    tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    EditorId,
) {
    use crate::render::hover::HoverPopup;
    use ratatui::style::Style;

    let mut h = Stoat::test();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    let root = PathBuf::from("/pool");
    let path = root.join("a.txt");
    h.fake_fs()
        .insert_file(&path, b"alpha\nbravo\ncharlie\ndelta\necho\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
    h.stoat.pending_hover = Some(HoverPopup::new(
        vec![vec![("hovered".to_string(), Style::default())]],
        0,
        editor_id,
    ));
    h.snapshot();
    (h, rx, editor_id)
}

#[test]
fn a_hover_inside_its_pane_ships_the_pool_anchor() {
    use stoatty_protocol::command::Command;

    let (mut h, mut rx, _) = hover_harness();
    let stamped = h
        .stoat
        .pending_hover
        .as_ref()
        .expect("popup")
        .anchor
        .expect("a popup inside the pane stamps an anchor");

    emit_smooth_scroll(&mut h.stoat);
    let batch = drain_apc(&mut rx);

    assert!(
        batch.iter().any(|cmd| matches!(
            cmd,
            Command::PoolAnchor(a)
                if a.pool == crate::smooth_scroll::non_pane_pool::HOVER
                    && (a.host, a.top_rows) == stamped
        )),
        "the hover pool names the pane it rides, got {batch:?}"
    );
}

/// A walkthrough card's box and body carry the pane's anchor themselves.
/// Its pool keeps the region for a scroll inside the card. A ride composites
/// the page's unfaded text before the pen closes the box.
#[test]
fn a_sketch_card_pools_its_body_without_riding_the_pane() {
    use stoatty_protocol::command::Command;

    let (mut h, mut rx, _) = hover_harness();
    h.stoat.pending_hover.as_mut().expect("popup").frame = HoverFrame::Sketch {
        id: 1,
        stroke: [0; 3],
        fill: [0; 3],
    };
    h.snapshot();
    assert!(
        h.stoat
            .pending_hover
            .as_ref()
            .expect("popup")
            .anchor
            .is_some(),
        "the card's body still stamps the pane it rides"
    );

    emit_smooth_scroll(&mut h.stoat);
    let batch = drain_apc(&mut rx);
    let hover_pool: Vec<&str> = batch
        .iter()
        .filter_map(|cmd| match cmd {
            Command::PoolRegion(r) if r.pool == crate::smooth_scroll::non_pane_pool::HOVER => {
                Some("region")
            },
            Command::PoolAnchor(_) => Some("anchor"),
            _ => None,
        })
        .collect();

    assert_eq!(
        hover_pool,
        ["region"],
        "the card declares its pool and rides nothing, got {batch:?}"
    );
}

#[test]
fn a_hover_overflowing_its_pane_ships_no_anchor() {
    use crate::render::hover::HoverPopup;
    use ratatui::style::Style;
    use stoatty_protocol::command::Command;

    let (mut h, mut rx, _) = hover_harness();
    // The popup is window-bounded, not pane-bounded, so a line wider than
    // half the terminal overflows a vertically split pane into its
    // neighbour. That is the case the anchor has to decline.
    action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
    let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
    h.stoat.pending_hover = Some(HoverPopup::new(
        vec![vec![("w".repeat(70), Style::default())]],
        0,
        editor_id,
    ));
    h.snapshot();

    assert_eq!(
        h.stoat.pending_hover.as_ref().expect("popup").anchor,
        None,
        "a popup past the pane's region stamps no anchor"
    );

    emit_smooth_scroll(&mut h.stoat);
    let batch = drain_apc(&mut rx);
    assert!(
        !batch
            .iter()
            .any(|cmd| matches!(cmd, Command::PoolAnchor(_))),
        "and nothing tells the terminal to ride it, got {batch:?}"
    );
}

#[test]
fn a_gliding_pane_freezes_the_hover_region_at_the_stamped_rect() {
    use stoatty_protocol::command::Command;

    let (mut h, mut rx, editor_id) = hover_harness();
    let stamped = h.stoat.pending_hover.as_ref().expect("popup").inner;

    // Arm a glide and jump the scroll target past where the popup was laid
    // out. Re-laying against that target is exactly what the freeze avoids.
    {
        let editor = h
            .stoat
            .active_workspace_mut()
            .editors
            .get_mut(editor_id)
            .expect("editor");
        editor.scroll_glide = ScrollGlide::Wheel;
        editor.scroll_row = 2;
    }

    emit_smooth_scroll(&mut h.stoat);
    let batch = drain_apc(&mut rx);

    let region = batch
        .iter()
        .find_map(|cmd| match cmd {
            Command::PoolRegion(r) if r.pool == crate::smooth_scroll::non_pane_pool::HOVER => {
                Some(*r)
            },
            _ => None,
        })
        .expect("the hover pool stays declared through the glide");

    assert_eq!(
        (region.top, region.left, region.width, region.height),
        (stamped.y, stamped.x, stamped.width, stamped.height),
        "the region holds the rect the live frame stamped"
    );
    assert!(
        !batch.iter().any(|cmd| matches!(
            cmd,
            Command::PoolDrop(d) if d.pool == crate::smooth_scroll::non_pane_pool::HOVER
        )),
        "and the pool stays in the active set, got {batch:?}"
    );
}

#[test]
fn apc_scene_emits_nothing_for_a_plain_editor_frame() {
    let mut h = Stoat::test();
    // The default theme resolves status colors to RGB, which drives the
    // status bar into the scene as sub-cell components. A theme without RGB
    // status colors keeps the status bar in cells, so with line numbers off
    // the frame stays genuinely widget-free.
    h.stoat.theme = Arc::new(rgb_review_theme());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    // Line numbers off so the paint carries no off-grid gutter, and the
    // minimap off so no strip declare rides the scene. Both keep the frame
    // genuinely widget-free.
    h.stoat.settings.editor_line_numbers = Some(LineNumbers::Off);
    h.stoat.settings.editor_minimap = Some(MinimapMode::Off);

    let root = PathBuf::from("/scene");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let mut buf = Buffer::empty(h.stoat.size());
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    assert!(
        drain_apc(&mut rx).is_empty(),
        "a widget-free paint appends nothing, so the scene flush stays silent"
    );
}

#[test]
fn status_bar_emits_sub_cell_components_inside_stoatty() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    // Line numbers off so the only off-grid components are the status bar's.
    h.stoat.settings.editor_line_numbers = Some(LineNumbers::Off);

    let root = PathBuf::from("/status");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let mut buf = Buffer::empty(h.stoat.size());
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::TextRun(t) if t.col == 0 && t.bg.is_none())),
        "the mode segment emits as a box-less text run at col 0, got {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::Bar(b) if b.height == 16)),
        "the mode segment background emits as a full-row bar, got {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::Bar(b) if b.height == 1)),
        "the status hairline emits as a one-sixteenth bar, got {cmds:?}"
    );
}

/// A bottom-right badge's panel covers the status row's right end, so the
/// rich bar's right segments end where the badge starts. The cursor position
/// is the rightmost of them, and a run past that edge is one the panel hides.
///
/// A frame before the badge rises records the bar's encoded frame. The badge
/// changes nothing else the bar draws from, so that record has to miss.
#[test]
fn rich_status_segments_end_where_a_corner_badge_starts() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.settings.editor_line_numbers = Some(LineNumbers::Off);

    let root = PathBuf::from("/status");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let mut buf = Buffer::empty(h.stoat.size());
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);
    drain_apc(&mut rx);

    h.stoat.active_workspace_mut().badges.insert(Badge {
        source: BadgeSource::Review,
        anchor: BadgeAnchor::BottomRight,
        state: BadgeState::Complete,
        label: "reviewing 1/3".to_owned(),
        detail: None,
    });
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    // The 15-wide box sits one column in from the right edge of 80, so it
    // starts at column 64. A glyph advances one cell under test.
    let cmds = drain_apc(&mut rx);
    let cursor_end = cmds.iter().find_map(|c| match c {
        Command::TextRun(t) if t.text == " 1:1 " => Some(t.col + 16 * t.text.len() as i16),
        _ => None,
    });
    assert_eq!(
        cursor_end,
        Some(64 * 16),
        "the cursor position ends at the badge's left edge, got {cmds:?}"
    );
}

/// The overlay screens' status row keeps its message clear of a bottom-right
/// badge the same way a pane's row does.
#[test]
fn overlay_status_message_ends_where_a_corner_badge_starts() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.active_workspace_mut().rebase = Some(crate::rebase::RebaseState::new(
        PathBuf::from("/overlay"),
        "onto".into(),
        vec![],
    ));
    h.stoat.set_status("no stops");
    h.stoat.active_workspace_mut().badges.insert(Badge {
        source: BadgeSource::Review,
        anchor: BadgeAnchor::BottomRight,
        state: BadgeState::Complete,
        label: "reviewing 1/3".to_owned(),
        detail: None,
    });

    let mut buf = Buffer::empty(h.stoat.size());
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    // The 15-wide box starts at column 64 of 80.
    let cmds = drain_apc(&mut rx);
    let message_end = cmds.iter().find_map(|c| match c {
        Command::TextRun(t) if t.text == "no stops" => Some(t.col + 16 * t.text.len() as i16),
        _ => None,
    });
    assert_eq!(
        message_end,
        Some(64 * 16),
        "the message ends at the badge's left edge, got {cmds:?}"
    );
}

#[test]
fn overlay_status_bar_emits_sub_cell_components_inside_stoatty() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    // A rebase overlay routes its status row through render_overlay_status.
    h.stoat.active_workspace_mut().rebase = Some(crate::rebase::RebaseState::new(
        PathBuf::from("/overlay"),
        "onto".into(),
        vec![],
    ));

    let mut buf = Buffer::empty(h.stoat.size());
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::TextRun(t) if t.col == 0)),
        "the overlay status row emits a text run at col 0, got {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::Bar(b) if b.height == 1)),
        "the overlay status hairline emits as a one-sixteenth bar, got {cmds:?}"
    );
}

#[test]
fn diagnostic_gutter_emits_sub_cell_bars_inside_stoatty() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    h.stoat.theme = Arc::new(rgb_diagnostic_theme());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/diag-rich");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\n");
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
            ..Default::default()
        }],
    );

    let mut buf = Buffer::empty(h.stoat.size());
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.iter().any(|c| matches!(c, Command::Bar(_))),
        "a severity mark emits a sub-cell bar, got {cmds:?}"
    );
}

#[test]
fn diagnostic_popover_emits_a_popover_frame_inside_stoatty() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    h.stoat.theme = Arc::new(rgb_diagnostic_theme());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/diag-popover");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"let x = 1;\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    // A span covering the start, so the cursor at offset zero sits inside it.
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
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "unexpected token".to_string(),
            ..Default::default()
        }],
    );

    let mut buf = Buffer::empty(h.stoat.size());
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.iter().any(|c| matches!(c, Command::Popover(_))),
        "a diagnostic under the cursor emits a popover frame, got {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::Icon(icon) if icon.offset == [3, 6])),
        "the severity icon carries the popover offset so it sits inside the card, got {cmds:?}"
    );
}

#[test]
fn pane_display_mode_renders_a_bold_digit_badge_per_pane() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    h.type_keys("space a s");
    assert_eq!(h.stoat.active_workspace().panes.pane_count(), 2);

    h.type_keys("space a e");
    let mut buf = Buffer::empty(h.stoat.size());
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let cmds = drain_apc(&mut rx);
    let popovers: Vec<_> = cmds
        .iter()
        .filter_map(|c| match c {
            Command::Popover(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(popovers.len(), 2, "each pane gets a badge, got {cmds:?}");
    assert!(popovers.iter().all(|p| p.bold), "badges shape bold");
    let digits: Vec<&str> = popovers
        .iter()
        .map(|p| buf[(p.left + 1, p.top + 1)].symbol())
        .collect();
    assert!(
        digits.contains(&"1") && digits.contains(&"2"),
        "the pane centers carry the digits 1 and 2, got {digits:?}"
    );

    let hint_box_shown = (buf.area.y..buf.area.y + buf.area.height).any(|y| {
        let row: String = (buf.area.x..buf.area.x + buf.area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect();
        row.contains(" space_pane_display ")
    });
    assert!(
        !hint_box_shown,
        "the chord suppresses the keybinding-hints box, leaving only the badges"
    );

    // Selecting a pane focuses it and returns to normal, so the next frame
    // draws no badges.
    h.type_keys("2");
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);
    assert!(
        !drain_apc(&mut rx)
            .iter()
            .any(|c| matches!(c, Command::Popover(_))),
        "selecting a pane clears the badges"
    );

    // Escape from the chord mode also clears the badges.
    h.type_keys("space a e");
    h.type_keys("escape");
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);
    assert!(
        !drain_apc(&mut rx)
            .iter()
            .any(|c| matches!(c, Command::Popover(_))),
        "escape clears the badges"
    );
}

#[test]
fn diagnostic_popover_dodges_a_cursor_under_the_below_placement() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    h.stoat.theme = Arc::new(rgb_diagnostic_theme());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/diag-popover-dodge");
    let path = root.join("a.txt");
    h.fake_fs()
        .insert_file(&path, b"aaaaa\nbbbbb\nccccc\nddddd\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    // A multi-line span keeps the cursor inside the diagnostic after it moves
    // down, and a multi-line message makes the below-anchor popover tall
    // enough to sit over the row beneath the diagnostic's start.
    h.seed_diagnostics(
        path,
        vec![lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 3,
                    character: 0,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "line one\nline two\nline three".to_string(),
            ..Default::default()
        }],
    );

    // Drop the cursor onto the row the below-anchor popover would occupy.
    action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveDown);

    let mut buf = Buffer::empty(h.stoat.size());
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let (cx, cy) = h
        .stoat
        .primary_cursor_screen_pos()
        .expect("primary cursor on screen");
    let cmds = drain_apc(&mut rx);
    let popover = cmds
        .iter()
        .find_map(|c| match c {
            Command::Popover(p) => Some(p),
            _ => None,
        })
        .expect("a diagnostic popover frame");

    let covers_cursor = cx >= popover.left
        && cx < popover.left + popover.width
        && cy >= popover.top
        && cy < popover.top + popover.height;
    assert!(
        !covers_cursor,
        "popover rect {popover:?} must not cover the cursor cell {:?}",
        (cx, cy)
    );
}

#[test]
fn help_modal_emits_a_panel_inside_stoatty() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
    h.stoat.theme = Arc::new(rgb_modal_theme());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
    h.settle();

    let size = h.stoat.size();
    let mut buf = Buffer::empty(size);
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let modal = help_layout(&h).modal;
    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Panel(p)
                if p.top == modal.y
                    && p.left == modal.x
                    && p.width == modal.width
                    && p.height == modal.height
        )),
        "the help modal emits a panel at its layout rect, got {cmds:?}"
    );
}

#[test]
fn help_modal_emits_a_panel_under_the_default_theme() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
    h.settle();

    let size = h.stoat.size();
    let mut buf = Buffer::empty(size);
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let modal = help_layout(&h).modal;
    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Panel(p)
                if p.top == modal.y
                    && p.left == modal.x
                    && p.width == modal.width
                    && p.height == modal.height
        )),
        "the shipped default theme resolves named colors to RGB, so the \
             help modal takes the rich arm and emits a panel, got {cmds:?}"
    );
}

#[test]
fn help_separator_emits_a_hairline_bar_inside_stoatty() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
    h.stoat.theme = Arc::new(rgb_modal_theme());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
    h.settle();

    let size = h.stoat.size();
    let mut buf = Buffer::empty(size);
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let list = help_layout(&h).list;
    let sep_x = (list.x + list.width) as i16 * 16 + 8;
    let sep_y = list.y as i16 * 16;
    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Bar(b)
                if b.x == sep_x
                    && b.y == sep_y
                    && b.width == 1
                    && b.height == list.height * 16
        )),
        "the help list/detail separator emits a hairline bar, got {cmds:?}"
    );
}

#[test]
fn hints_overlay_emits_scaled_text_runs_inside_stoatty() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    h.stoat.theme = Arc::new(rgb_modal_theme());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
    h.settle();

    let mut buf = Buffer::empty(h.stoat.size());
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::TextRun(t) if t.scale == 218)),
        "the hints overlay emits 0.85x hint-row text runs, got {cmds:?}"
    );
}

#[test]
fn the_hints_box_anchors_to_the_bottom_right_corner() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    h.stoat.theme = Arc::new(rgb_modal_theme());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.type_keys("space");

    let size = h.stoat.size();
    let mut buf = Buffer::empty(size);
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let panel = drain_apc(&mut rx)
        .into_iter()
        .find_map(|c| match c {
            Command::Panel(p) => Some(p),
            _ => None,
        })
        .expect("the standing hints box emits a panel");
    assert_eq!(
        panel.top + panel.height,
        size.height - 1,
        "the hints box's bottom edge sits just above the reserved status row"
    );
    assert_eq!(
        panel.left + panel.width,
        size.width,
        "the hints box's right edge lands on the window's last column"
    );
}

/// The box is declared after every modal, so it lands over the commit
/// picker's pooled list and preview. Layered with the grid it would be
/// painted over by their composites for the length of every glide, which is
/// what made it blink out mid-scroll.
#[test]
fn the_hints_box_panel_floats_above_pooled_surfaces() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    h.stoat.theme = Arc::new(rgb_modal_theme());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.type_keys("space");

    let mut buf = Buffer::empty(h.stoat.size());
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let flags: Vec<bool> = drain_apc(&mut rx)
        .into_iter()
        .filter_map(|c| match c {
            Command::Panel(p) => Some(p.above_pools),
            _ => None,
        })
        .collect();
    assert_eq!(
        flags,
        [true],
        "the standing hints box is the frame's one panel, flagged above pools"
    );
}

#[test]
fn hover_body_emits_scaled_text_runs_inside_stoatty() {
    use lsp_types::{HoverProviderCapability, ServerCapabilities};
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.fake_lsp().set_capabilities(ServerCapabilities {
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        ..Default::default()
    });

    let root = PathBuf::from("/hover-apc");
    let path = root.join("main.rs");
    h.fake_fs().insert_file(&path, b"fn foo() {}\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();

    h.fake_lsp()
        .set_hover(path.to_str().unwrap(), 0, 0, "fn foo() -> u32");
    action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
    h.settle();

    let mut buf = Buffer::empty(h.stoat.size());
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::TextRun(t) if t.scale == 218)),
        "the hover body emits 0.85x scaled text runs under stoatty, got {cmds:?}"
    );
}

#[test]
fn pane_divider_emits_a_hairline_bar_inside_stoatty() {
    use crate::pane::DividerOrientation;
    use stoatty_protocol::command::{BarCommand, Command};

    let mut h = Stoat::test();
    h.stoat.theme = Arc::new(rgb_border_theme());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);

    let size = h.stoat.size();
    let mut buf = Buffer::empty(size);
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let dividers = h.stoat.active_workspace().panes.dividers();
    let d = dividers
        .iter()
        .find(|d| matches!(d.orientation, DividerOrientation::Vertical))
        .expect("the split has a vertical divider");
    let end_y = d.y.saturating_add(d.len).min(size.height);
    let expected = BarCommand {
        x: d.x as i16 * 16 + 8,
        y: d.y as i16 * 16,
        width: 1,
        height: (end_y - d.y) * 16,
        color: if d.touches_focus {
            [0xaa, 0xbb, 0xcc]
        } else {
            [0x55, 0x66, 0x77]
        },
    };
    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.contains(&Command::Bar(expected)),
        "the split divider emits a hairline bar in the border color, got {cmds:?}"
    );
}

#[test]
fn completion_popup_emits_a_panel_inside_stoatty() {
    use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    h.stoat.theme = Arc::new(rgb_modal_theme());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/complete");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    h.type_keys("i");

    h.stoat.pending_completion = Some(CompletionPopup::showing(vec![CompletionItem {
        label: "println".into(),
        source: CompletionSource::Lsp,
        kind: None,
        detail: None,
        replace_range: crate::completion::unused_replace_range(),
        insert_text: "println".into(),
        is_snippet: false,
        documentation: None,
        lsp_item: None,
        server: None,
    }]));

    let mut buf = Buffer::empty(h.stoat.size());
    app::paint_frame(&mut h.stoat, &mut buf);
    emit_apc_scene(&mut h.stoat);

    let popup_area = crate::render::completion::completion_popup_layout(&mut h.stoat)
        .expect("completion popup lays out")
        .1
        .popup_area;
    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Panel(p)
                if p.top == popup_area.y
                    && p.left == popup_area.x
                    && p.width == popup_area.width
                    && p.height == popup_area.height
        )),
        "the completion popup emits a panel at its layout rect, got {cmds:?}"
    );
}

#[test]
fn editor_pool_pages_fill_asynchronously() {
    use stoatty_protocol::command::{Command, FillCommand};

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/async-pool");
    let path = root.join("a.txt");
    let body = (0..150).map(|i| format!("line {i}\n")).collect::<String>();
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    emit_smooth_scroll(&mut h.stoat);

    // The first batch carries the pool geometry and scroll, but no editor fill:
    // plain editor pages are rendered off the run loop, not inline.
    let first = rx.try_recv().expect("region/scroll batch");
    let first_cmds = command::decode_stream(&first);
    assert!(
        first_cmds
            .iter()
            .any(|c| matches!(c, Command::PoolRegion(_))),
        "first batch declares the pool, got {first_cmds:?}"
    );
    assert!(
        !first_cmds.iter().any(|c| matches!(c, Command::Fill(_))),
        "first batch carries no synchronous editor fill, got {first_cmds:?}"
    );

    // The blocking renders run inline under the test scheduler, so their fills
    // arrive as later batches on the same channel. The initial visible page is 0,
    // whose buffered window is pages 0..5.
    let mut filled = Vec::new();
    while let Ok(batch) = rx.try_recv() {
        for cmd in command::decode_stream(&batch) {
            if let Command::Fill(FillCommand { index, .. }) = cmd {
                filled.push(index);
            }
        }
    }
    filled.sort_unstable();
    assert_eq!(
        filled,
        vec![0, 1, 2, 3, 4],
        "the initial window's pages fill asynchronously, got {filled:?}"
    );

    fn drain_fills(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<u64> {
        let mut filled = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            for cmd in command::decode_stream(&batch) {
                if let Command::Fill(FillCommand { index, .. }) = cmd {
                    filled.push(index);
                }
            }
        }
        filled.sort_unstable();
        filled
    }

    // A resize moves the pane's rectangle, and its next event would drop
    // whatever page this one rendered, so the pool holds the page back.
    let size = h.stoat.size();
    h.resize(size.width, size.height - 2);
    emit_smooth_scroll(&mut h.stoat);
    assert_eq!(
        drain_fills(&mut rx),
        Vec::<u64>::new(),
        "the resize frame renders no page"
    );

    // The settle frame finds the rectangle where the resize left it, and a
    // resting pool fills the visible page alone.
    h.advance_clock(debounce::POOL_SETTLE_DEBOUNCE);
    emit_smooth_scroll(&mut h.stoat);
    assert_eq!(
        drain_fills(&mut rx),
        vec![0],
        "the settle frame fills the page the resize held back"
    );
}

#[test]
fn detached_editor_ships_a_window_bound_pool() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.window_ipc_connected = true;

    let root = PathBuf::from("/detach-emit");
    let path = root.join("a.txt");
    let body = (0..150).map(|i| format!("line {i}\n")).collect::<String>();
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);
    h.type_action("SplitRight()");
    h.settle();
    h.type_action("DetachPane()");
    while rx.try_recv().is_ok() {}

    emit_smooth_scroll(&mut h.stoat);
    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::PoolRegion(r) if r.window != 0)),
        "the detached editor declares a window-bound pool, got {cmds:?}"
    );
}

#[test]
fn detached_focus_ships_a_pool_cursor_that_goes_quiet_when_idle() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.window_ipc_connected = true;

    let root = PathBuf::from("/detach-cursor");
    let path = root.join("a.txt");
    let body = (0..30).map(|i| format!("line {i}\n")).collect::<String>();
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);
    h.type_action("SplitRight()");
    h.settle();
    h.type_action("DetachPane()");
    while rx.try_recv().is_ok() {}

    emit_smooth_scroll(&mut h.stoat);
    assert!(
        drain_apc(&mut rx)
            .iter()
            .any(|c| matches!(c, Command::PoolCursor(_))),
        "the detached focus ships a pool cursor"
    );

    emit_smooth_scroll(&mut h.stoat);
    assert!(
        !drain_apc(&mut rx)
            .iter()
            .any(|c| matches!(c, Command::PoolCursor(_))),
        "an unchanged frame ships no pool cursor"
    );
}

/// Acting on an unchanged version now skips `emit_into` outright, and with
/// it the region declaration that used to go out regardless. So the region
/// has to be part of what the version is computed from.
///
/// Only the row count changes here. The segments are laid out against the
/// row's width, so they come back identical and the region is the only thing
/// that moved.
#[test]
fn a_moved_status_row_redeclares_its_region_though_its_segments_match() {
    use stoatty_protocol::command::Command;

    let status_base = crate::smooth_scroll::non_pane_pool::WINDOW_STATUS;
    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.window_ipc_connected = true;

    let root = PathBuf::from("/status-move");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"hello\nworld\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);
    h.type_action("SplitRight()");
    h.settle();
    h.type_action("DetachPane()");

    let panes = &h.stoat.active_workspace().panes;
    let window = match &panes.pane(panes.focus()).placement {
        Placement::Window(window) => *window,
        other => panic!("the pane detaches into a window, got {other:?}"),
    };
    app::deliver_window_event(
        &mut h.stoat,
        WindowIpcEvent::Resized {
            window,
            cols: 80,
            rows: 24,
        },
    );
    emit_smooth_scroll(&mut h.stoat);
    while rx.try_recv().is_ok() {}

    emit_smooth_scroll(&mut h.stoat);
    assert!(
        !drain_apc(&mut rx)
            .iter()
            .any(|c| matches!(c, Command::PoolRegion(r) if r.pool >= status_base)),
        "an unchanged status row re-declares nothing"
    );

    app::deliver_window_event(
        &mut h.stoat,
        WindowIpcEvent::Resized {
            window,
            cols: 80,
            rows: 18,
        },
    );
    emit_smooth_scroll(&mut h.stoat);
    let moved = drain_apc(&mut rx);
    assert!(
        moved
            .iter()
            .any(|c| matches!(c, Command::PoolRegion(r) if r.pool >= status_base)),
        "a shorter window moves the status row up, got {moved:?}"
    );
}

#[test]
fn detached_pane_ships_a_window_status_row_that_goes_quiet_when_idle() {
    use stoatty_protocol::command::Command;

    let status_base = crate::smooth_scroll::non_pane_pool::WINDOW_STATUS;
    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.window_ipc_connected = true;

    let root = PathBuf::from("/detach-status");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"hello\nworld\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);
    h.type_action("SplitRight()");
    h.settle();
    h.type_action("DetachPane()");
    while rx.try_recv().is_ok() {}

    emit_smooth_scroll(&mut h.stoat);
    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::PoolRegion(r) if r.pool >= status_base && r.window != 0)),
        "the detached pane declares a window-bound status pool, got {cmds:?}"
    );

    emit_smooth_scroll(&mut h.stoat);
    assert!(
        !drain_apc(&mut rx)
            .iter()
            .any(|c| matches!(c, Command::PoolRegion(r) if r.pool >= status_base)),
        "an unchanged status bar re-declares nothing"
    );
}

#[test]
fn detached_terminal_ships_a_content_pool_that_repaints_then_goes_quiet() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.window_ipc_connected = true;
    h.resize(80, 24);
    h.type_action("SplitRight()");
    h.settle();

    // Point the focused split pane at a terminal without leaving normal
    // mode, so DetachPane rides its keybinding as a user would.
    let session: Arc<dyn crate::host::TerminalSession> =
        Arc::new(crate::host::FakeTerminalSession::new());
    let (focused, term_id, index) = {
        let ws = h.stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let term_id = ws.terms.insert(TermSession::new(
            crate::term_screen::TermScreen::new(24, 80),
            session,
            TermSession::next_token(),
        ));
        ws.panes.pane_mut(focused).view = View::Terminal(term_id);
        (focused, term_id, ws.panes.pane(focused).index)
    };

    h.type_action("DetachPane()");
    assert!(
        matches!(
            h.stoat.active_workspace().panes.pane(focused).placement,
            Placement::Window(_)
        ),
        "a terminal pane detaches into its own window"
    );
    while rx.try_recv().is_ok() {}

    emit_smooth_scroll(&mut h.stoat);
    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::PoolRegion(r) if r.pool == index && r.window != 0)),
        "the detached terminal declares a window-bound content pool, got {cmds:?}"
    );

    emit_smooth_scroll(&mut h.stoat);
    let idle = drain_apc(&mut rx);
    assert!(
        !idle
            .iter()
            .any(|c| matches!(c, Command::PoolRegion(r) if r.pool == index)),
        "an idle terminal re-declares no content pool, got {idle:?}"
    );

    h.stoat
        .active_workspace_mut()
        .terms
        .get_mut(term_id)
        .expect("terminal session")
        .term
        .feed(b"detached output");
    emit_smooth_scroll(&mut h.stoat);
    let after = drain_apc(&mut rx);
    assert!(
        after
            .iter()
            .any(|c| matches!(c, Command::Fill(f) if f.pool == index)),
        "terminal output repaints the content pool, got {after:?}"
    );
}

#[test]
fn focus_pane_by_number_reaches_and_raises_a_detached_window() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.window_ipc_connected = true;
    h.resize(80, 24);
    h.type_action("SplitRight()");
    h.settle();
    h.type_action("DetachPane()");
    h.settle();

    let detached = h.stoat.active_workspace().panes.windowed_panes()[0].0;
    action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusPane { index: 1 });
    while rx.try_recv().is_ok() {}

    action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusPane { index: 2 });
    assert_eq!(
        h.stoat.active_workspace().panes.focus(),
        detached,
        "FocusPane reaches the detached pane at selectable position 2"
    );
    assert!(
        drain_apc(&mut rx)
            .iter()
            .any(|c| matches!(c, Command::WindowFocus(_))),
        "focusing a detached pane raises its OS window"
    );
}

#[test]
fn detached_pane_status_badge_continues_the_split_count() {
    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.window_ipc_connected = true;
    h.resize(80, 24);
    h.type_action("SplitRight()");
    h.settle();
    h.type_action("DetachPane()");
    h.settle();

    h.stoat.set_focused_mode("space_pane_display".to_string());
    while rx.try_recv().is_ok() {}
    emit_smooth_scroll(&mut h.stoat);

    let mut bytes = Vec::new();
    while let Ok(chunk) = rx.try_recv() {
        bytes.extend(chunk);
    }
    assert!(
        bytes.windows(3).any(|w| w == b"[2]"),
        "a detached pane's status badge continues the split count to 2"
    );
}

#[test]
fn the_modal_preview_is_pooled_and_retired() {
    use stoat_action::OpenFileFinder;
    use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/preview-pool");
    for name in ["a.rs", "b.rs", "c.rs"] {
        h.fake_fs()
            .insert_file(root.join(name), b"one\ntwo\nthree\n");
    }
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    emit_smooth_scroll(&mut h.stoat);
    let preview = finder_layout(&h)
        .preview
        .expect("the test terminal is wide enough for a preview");
    let expected = PoolRegionCommand {
        pool: crate::smooth_scroll::non_pane_pool::MODAL_PREVIEW,
        top: preview.y,
        left: preview.x,
        width: preview.width,
        height: preview.height,
        window: 0,
    };
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
        "the finder preview declares a pool at its preview rect"
    );

    // The pool has to stay in the active set while the modal is up, or the
    // next frame retires it out from under the open preview.
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        !drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
            pool: crate::smooth_scroll::non_pane_pool::MODAL_PREVIEW,
        })),
        "and keeps it across a frame the modal stays open for"
    );

    h.stoat.file_finder = None;
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
            pool: crate::smooth_scroll::non_pane_pool::MODAL_PREVIEW,
        })),
        "closing the finder retires its preview pool"
    );
}

/// The whole point of the pool: a wheel over the preview has to move the
/// pool's scroll target, or the terminal has nothing to ease toward and the
/// glide paints nothing.
#[test]
fn a_wheel_over_the_preview_moves_the_pool_scroll_target() {
    use crossterm::event::{MouseEvent, MouseEventKind};
    use stoat_action::OpenFileFinder;
    use stoatty_protocol::command::Command;

    // A short glide moves within one page, so the fraction carries it. The
    // page index alone reads as standing still.
    fn scroll_targets(cmds: &[Command]) -> Vec<(u64, u16)> {
        cmds.iter()
            .filter_map(|cmd| match cmd {
                Command::Scroll(scroll)
                    if scroll.pool == crate::smooth_scroll::non_pane_pool::MODAL_PREVIEW =>
                {
                    Some((scroll.page, scroll.fraction))
                },
                _ => None,
            })
            .collect()
    }

    let mut h = crate::test_harness::TestHarness::with_size(160, 40);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/preview-wheel");
    let long: String = (0..400).map(|i| format!("line {i}\n")).collect();
    h.fake_fs().insert_file(root.join("a.rs"), long.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
    h.settle();
    h.stoat.drive_background();
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    emit_smooth_scroll(&mut h.stoat);
    let before = scroll_targets(&drain_apc(&mut rx));

    let preview = finder_layout(&h)
        .preview
        .expect("the test terminal is wide enough for a preview");
    for _ in 0..12 {
        h.stoat.update(crossterm::event::Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: preview.x + 2,
            row: preview.y + 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }));
    }
    h.settle();
    // The wheel arms a glide and the pool follows the eased offset, not the
    // jumped row, so the target only moves once the animation ticks.
    h.stoat.tick_scroll_anim(0.016);

    emit_smooth_scroll(&mut h.stoat);
    let after = scroll_targets(&drain_apc(&mut rx));
    assert!(
        after.last() > before.last(),
        "the wheel moved the pool target past {before:?}, got {after:?}"
    );
}
#[test]
fn file_finder_list_is_pooled_and_retired() {
    use stoat_action::OpenFileFinder;
    use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/finder");
    for name in ["a.rs", "b.rs", "c.rs"] {
        h.fake_fs().insert_file(root.join(name), b"x\n");
    }
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    emit_smooth_scroll(&mut h.stoat);
    let list = finder_layout(&h).list;
    let expected = PoolRegionCommand {
        pool: crate::smooth_scroll::non_pane_pool::FINDER,
        top: list.y,
        left: list.x,
        width: list.width,
        height: list.height,
        window: 0,
    };
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
        "the finder list declares a pool at its list rect"
    );

    h.stoat.file_finder = None;
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
            pool: crate::smooth_scroll::non_pane_pool::FINDER,
        })),
        "closing the finder retires its pool"
    );
}

#[test]
fn code_search_list_is_pooled_and_retired() {
    use stoat_action::OpenCodeSearch;
    use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/codesearch");
    h.fake_fs().insert_file(root.join("a.rs"), b"needle\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenCodeSearch);
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    emit_smooth_scroll(&mut h.stoat);
    let list = crate::render::file_finder::file_finder_layout(
        size,
        (u16::MAX, u16::MAX),
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::CodeSearch),
        modal_split_percent(&h.stoat.modal_split, ModalKind::CodeSearch),
    )
    .expect("code search fits the test terminal")
    .list;
    let expected = PoolRegionCommand {
        pool: crate::smooth_scroll::non_pane_pool::CODE_SEARCH_LIST,
        top: list.y,
        left: list.x,
        width: list.width,
        height: list.height,
        window: 0,
    };
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
        "the match list declares a pool at its list rect"
    );

    // The pool has to stay in the active set while the modal is up, or the
    // next frame retires it out from under the open list.
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        !drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
            pool: crate::smooth_scroll::non_pane_pool::CODE_SEARCH_LIST,
        })),
        "and keeps it across a frame the modal stays open for"
    );

    h.stoat.code_search = None;
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
            pool: crate::smooth_scroll::non_pane_pool::CODE_SEARCH_LIST,
        })),
        "closing code search retires its pool"
    );
}

/// A landed scan plus a selection past the visible page refills the pool.
///
/// Both halves are load-bearing. The pool holds its refill while the scroll
/// target sits still, since the live grid paints those rows anyway, so a
/// version that never moved would also emit nothing here. Scrolling past the
/// page is what makes the deferred window observable.
#[test]
fn a_scrolled_code_search_list_refills_its_pool() {
    use stoat_action::OpenCodeSearch;
    use stoatty_protocol::command::Command;

    fn fills(cmds: &[Command]) -> Vec<u64> {
        cmds.iter()
            .filter_map(|cmd| match cmd {
                Command::Fill(fill)
                    if fill.pool == crate::smooth_scroll::non_pane_pool::CODE_SEARCH_LIST =>
                {
                    Some(fill.index)
                },
                _ => None,
            })
            .collect()
    }

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/codesearch-fill");
    for i in 0..200 {
        h.fake_fs()
            .insert_file(root.join(format!("f{i}.rs")), b"needle here\n");
    }
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenCodeSearch);
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    h.type_text("needle");
    h.settle();
    h.advance_clock(debounce::CODE_SEARCH_DEBOUNCE);
    h.settle();
    let landed = h.stoat.code_search.as_ref().expect("open").matches.len();
    assert!(landed > 100, "the scan landed matches to scroll: {landed}");

    // The first display prefills the whole window, which is the state the
    // scroll arrives into.
    emit_smooth_scroll(&mut h.stoat);
    let _ = drain_apc(&mut rx);

    let finder = h.stoat.code_search.as_mut().expect("open");
    finder.selected = landed - 1;
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        !fills(&drain_apc(&mut rx)).is_empty(),
        "a selection past the visible page refills the pool"
    );
}

#[test]
fn symbol_finder_list_is_pooled_and_retired() {
    use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    crate::test_fixture::enable_document_symbols(&h);
    let root = crate::test_fixture::seed(&mut h, &[("main.rs", "fn one() {}\nfn two() {}\n")]);
    let path = root.join("main.rs");
    crate::test_fixture::open_buffer(&mut h, path.clone());
    h.fake_lsp().set_document_symbols(
        path.to_str().unwrap(),
        lsp_types::DocumentSymbolResponse::Flat(vec![
            crate::test_fixture::flat_symbol("one", path.to_str().unwrap(), 0, 3),
            crate::test_fixture::flat_symbol("two", path.to_str().unwrap(), 1, 3),
        ]),
    );
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    emit_smooth_scroll(&mut h.stoat);
    let list = {
        let finder = h.stoat.symbol_finder.as_ref().expect("the finder is open");
        crate::render::symbol_finder::symbol_finder_layout(
            size,
            finder.content_rows,
            modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::SymbolFinder),
            modal_split_percent(&h.stoat.modal_split, ModalKind::SymbolFinder),
        )
        .expect("the finder fits the test terminal")
        .2
    };
    let expected = PoolRegionCommand {
        pool: crate::smooth_scroll::non_pane_pool::SYMBOL_FINDER_LIST,
        top: list.y,
        left: list.x,
        width: list.width,
        height: list.height,
        window: 0,
    };
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
        "the symbol list declares a pool at its list rect"
    );

    // The pool has to stay in the active set while the modal is up, or the
    // next frame retires it out from under the open list.
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        !drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
            pool: crate::smooth_scroll::non_pane_pool::SYMBOL_FINDER_LIST,
        })),
        "and keeps it across a frame the modal stays open for"
    );

    h.stoat.symbol_finder = None;
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
            pool: crate::smooth_scroll::non_pane_pool::SYMBOL_FINDER_LIST,
        })),
        "closing the symbol finder retires its pool"
    );
}
/// The pool paints rows the live grid paints anyway, so a keystroke that
/// re-filters the list has nothing worth filling until the target moves.
///
/// Both halves are load-bearing. A pool that stopped refilling altogether
/// would satisfy the first assertion, so the second moves the selection past
/// the visible page and requires the deferred window back.
#[test]
fn typing_in_the_finder_defers_its_pool_refill_until_the_list_scrolls() {
    use stoat_action::OpenFileFinder;
    use stoatty_protocol::command::Command;

    fn finder_fills(cmds: &[Command]) -> Vec<u64> {
        cmds.iter()
            .filter_map(|cmd| match cmd {
                Command::Fill(fill) if fill.pool == crate::smooth_scroll::non_pane_pool::FINDER => {
                    Some(fill.index)
                },
                _ => None,
            })
            .collect()
    }

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/holdfinder");
    for i in 0..200 {
        h.fake_fs()
            .insert_file(root.join(format!("a{i}.rs")), b"x\n");
    }
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    // The first display prefills the whole window, which is the state a
    // keystroke arrives into.
    emit_smooth_scroll(&mut h.stoat);
    let _ = drain_apc(&mut rx);

    h.type_text("a");
    h.settle();
    emit_smooth_scroll(&mut h.stoat);
    let typed = finder_fills(&drain_apc(&mut rx));
    assert!(
        typed.is_empty(),
        "a keystroke into a resting finder fills no page, got {typed:?}"
    );

    // Selecting past the visible page moves the scroll target, which is
    // where the deferred content change applies.
    let list_height = finder_layout(&h).list.height as usize;
    h.stoat
        .file_finder
        .as_mut()
        .expect("the finder is open")
        .active_core()
        .picklist
        .selected = list_height;
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        !finder_fills(&drain_apc(&mut rx)).is_empty(),
        "the deferred window fills once the list scrolls"
    );
}

#[test]
fn finder_pool_region_spans_the_full_window_over_the_band() {
    use stoat_action::OpenFileFinder;
    use stoatty_protocol::command::{Command, PoolRegionCommand};

    let mut h = Stoat::test();
    h.resize(120, 24);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/bandfinder");
    for name in ["a.rs", "b.rs", "c.rs"] {
        h.fake_fs().insert_file(root.join(name), b"x\n");
    }
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);

    // Paint one frame so the single-minimap band is stamped. Single is the
    // default mode and the test terminal is wide enough to reserve the strip.
    h.snapshot();
    assert_ne!(
        h.stoat.layout_size(),
        h.stoat.size(),
        "the single-minimap band must be reserved so the test proves the modal ignores it"
    );
    let _ = drain_apc(&mut rx);

    emit_smooth_scroll(&mut h.stoat);
    let list = finder_layout(&h).list;
    let expected = PoolRegionCommand {
        pool: crate::smooth_scroll::non_pane_pool::FINDER,
        top: list.y,
        left: list.x,
        width: list.width,
        height: list.height,
        window: 0,
    };
    assert!(
            drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
            "the finder pool spans the full window even with the band reserved, so the modal takes precedence over the strip"
        );
}

#[test]
fn palette_list_is_pooled_and_retired() {
    use stoat_action::OpenCommandPalette;
    use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    action_handlers::dispatch(&mut h.stoat, &OpenCommandPalette);
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    emit_smooth_scroll(&mut h.stoat);
    let (rows, zoom) = palette_sizing(&h);
    let list = crate::render::command_palette::palette_filter_layout(h.stoat.size(), rows, zoom)
        .expect("the palette fits the test terminal")
        .list;
    let expected = PoolRegionCommand {
        pool: crate::smooth_scroll::non_pane_pool::PALETTE,
        top: list.y,
        left: list.x,
        width: list.width,
        height: list.height,
        window: 0,
    };
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
        "the palette list declares a pool at its list rect"
    );

    h.stoat.command_palette = None;
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
            pool: crate::smooth_scroll::non_pane_pool::PALETTE,
        })),
        "closing the palette retires its pool"
    );
}

#[test]
fn palette_arg_list_is_pooled_and_retired() {
    use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/argpool");
    for name in ["a.rs", "b.rs", "c.rs"] {
        h.fake_fs().insert_file(root.join(name), b"x\n");
    }
    h.stoat.active_workspace_mut().git_root = root;
    // Typing `:o ` opens the palette and installs the Files arg picker. The
    // snapshot drives drive_background so the picker is live before emit.
    h.type_text(":o ");
    h.snapshot();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);
    let _ = drain_apc(&mut rx);

    emit_smooth_scroll(&mut h.stoat);
    let (rows, zoom) = palette_sizing(&h);
    let list = crate::render::command_palette::palette_arg_list_rect(h.stoat.size(), rows, zoom)
        .expect("the arg picker fits the test terminal");
    let expected = PoolRegionCommand {
        pool: crate::smooth_scroll::non_pane_pool::PALETTE,
        top: list.y,
        left: list.x,
        width: list.width,
        height: list.height,
        window: 0,
    };
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
        "the arg-picker list declares a pool at its list rect"
    );

    h.stoat.command_palette = None;
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
            pool: crate::smooth_scroll::non_pane_pool::PALETTE,
        })),
        "closing the palette retires its pool"
    );
}

#[test]
fn palette_filter_to_arg_flip_repools() {
    use stoat_action::OpenCommandPalette;
    use stoatty_protocol::command::{Command, PoolRegionCommand};

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/argflip");
    for name in ["a.rs", "b.rs"] {
        h.fake_fs().insert_file(root.join(name), b"x\n");
    }
    h.stoat.active_workspace_mut().git_root = root;

    // Filter mode holds the PALETTE pool through the command list.
    action_handlers::dispatch(&mut h.stoat, &OpenCommandPalette);
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);
    emit_smooth_scroll(&mut h.stoat);
    let _ = drain_apc(&mut rx);

    // Flipping to arg mode re-declares the same pool at the arg-list rect.
    h.type_text("o ");
    h.snapshot();
    h.stoat.active_workspace_mut().layout(size);
    emit_smooth_scroll(&mut h.stoat);

    let (rows, zoom) = palette_sizing(&h);
    let arg_list =
        crate::render::command_palette::palette_arg_list_rect(h.stoat.size(), rows, zoom)
            .expect("the arg picker fits the test terminal");
    let expected = PoolRegionCommand {
        pool: crate::smooth_scroll::non_pane_pool::PALETTE,
        top: arg_list.y,
        left: arg_list.x,
        width: arg_list.width,
        height: arg_list.height,
        window: 0,
    };
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
        "flipping filter to arg mode re-declares the pool at the arg-list rect"
    );
}

#[test]
fn completion_popup_is_pooled_and_retired() {
    use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
    use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    let item = |label: &str| CompletionItem {
        label: label.into(),
        source: CompletionSource::Lsp,
        kind: None,
        detail: None,
        replace_range: crate::completion::unused_replace_range(),
        insert_text: label.into(),
        is_snippet: false,
        documentation: None,
        lsp_item: None,
        server: None,
    };
    h.stoat.pending_completion = Some(CompletionPopup::showing(vec![
        item("alpha"),
        item("beta"),
        item("gamma"),
    ]));

    emit_smooth_scroll(&mut h.stoat);
    let (_, layout) = crate::render::completion::completion_popup_layout(&mut h.stoat)
        .expect("the popup anchors in the test terminal");
    let expected = PoolRegionCommand {
        pool: crate::smooth_scroll::non_pane_pool::COMPLETION,
        top: layout.inner.y,
        left: layout.inner.x,
        width: layout.inner.width,
        height: layout.inner.height,
        window: 0,
    };
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
        "the completion popup declares a pool at its inner rect"
    );

    h.stoat.pending_completion = None;
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
            pool: crate::smooth_scroll::non_pane_pool::COMPLETION,
        })),
        "closing the popup retires its pool"
    );
}

#[test]
fn help_list_and_detail_are_pooled_and_retired() {
    use stoat_action::OpenHelp;
    use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    action_handlers::dispatch(&mut h.stoat, &OpenHelp);
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    emit_smooth_scroll(&mut h.stoat);
    let layout = help_layout(&h);
    let list = PoolRegionCommand {
        pool: crate::smooth_scroll::non_pane_pool::HELP_LIST,
        top: layout.list.y,
        left: layout.list.x,
        width: layout.list.width,
        height: layout.list.height,
        window: 0,
    };
    let detail = PoolRegionCommand {
        pool: crate::smooth_scroll::non_pane_pool::HELP_DETAIL,
        top: layout.detail.y,
        left: layout.detail.x,
        width: layout.detail.width,
        height: layout.detail.height,
        window: 0,
    };
    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.contains(&Command::PoolRegion(list)),
        "the help list declares a pool at its rect"
    );
    assert!(
        cmds.contains(&Command::PoolRegion(detail)),
        "the help detail declares a pool at its rect"
    );

    h.stoat.help = None;
    emit_smooth_scroll(&mut h.stoat);
    let cmds = drain_apc(&mut rx);
    assert!(
        cmds.contains(&Command::PoolDrop(PoolDropCommand {
            pool: crate::smooth_scroll::non_pane_pool::HELP_LIST,
        })),
        "closing help retires the list pool"
    );
    assert!(
        cmds.contains(&Command::PoolDrop(PoolDropCommand {
            pool: crate::smooth_scroll::non_pane_pool::HELP_DETAIL,
        })),
        "closing help retires the detail pool"
    );
}

#[test]
fn commits_list_is_pooled_and_retired() {
    use crate::commit_list::CommitListState;
    use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);
    let repo = h
        .stoat
        .git_host
        .discover(std::path::Path::new("/work"))
        .unwrap_or_else(|| {
            h.fake_git.add_repo("/work");
            h.stoat
                .git_host
                .discover(std::path::Path::new("/work"))
                .expect("the repo was just added")
        });
    h.stoat.active_workspace_mut().commits =
        Some(CommitListState::new(PathBuf::from("/work"), repo));
    h.stoat.set_focused_mode("commits".to_string());

    emit_smooth_scroll(&mut h.stoat);
    let focused = {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).area
    };
    let list = crate::render::commits::commits_list_rect(focused, h.stoat.commits_split)
        .expect("the commits list fits the test terminal");
    let expected = PoolRegionCommand {
        pool: crate::smooth_scroll::non_pane_pool::COMMITS,
        top: list.y,
        left: list.x,
        width: list.width,
        height: list.height,
        window: 0,
    };
    let bytes = rx
        .try_recv()
        .expect("the commits overlay emits an APC batch");
    assert!(
        command::decode_stream(&bytes).contains(&Command::PoolRegion(expected)),
        "the commits list declares a pool at its list rect"
    );

    h.stoat.set_focused_mode("normal".to_string());
    h.stoat.active_workspace_mut().commits = None;
    emit_smooth_scroll(&mut h.stoat);
    let bytes = rx.try_recv().expect("leaving commits emits a drop");
    assert!(
        command::decode_stream(&bytes).contains(&Command::PoolDrop(PoolDropCommand {
            pool: crate::smooth_scroll::non_pane_pool::COMMITS,
        })),
        "leaving commits mode retires its pool"
    );
}

/// A conflict pane is a `View::Editor`, so without its own fill arm it
/// routes to the plain-editor page and its pooled fills paint the bare
/// center scratch buffer, dropping the flanking columns wherever a pool page
/// covers the region. Pins the routing rather than the page renderer.
#[test]
fn a_conflict_pane_fills_with_the_three_column_body() {
    use crate::render::conflict_view::render_conflict_rows;
    use ratatui::layout::Rect;

    let mut h = crate::test_harness::TestHarness::with_size(150, 24);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_git()
        .add_repo("/repo")
        .with_fs(h.fake_fs())
        .conflicted_file(
            "f.txt",
            Some("a\nbase\nz\n"),
            Some("a\nOURS\nz\n"),
            Some("a\nTHEIRS\nz\n"),
        );
    action_handlers::dispatch(&mut h.stoat, &Conflict);
    h.settle();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    emit_smooth_scroll(&mut h.stoat);
    // The fills render on blocking workers, so settle before draining.
    h.settle();

    let (_, _, region) = editor_pool_panes(&h.stoat)
        .into_iter()
        .next()
        .expect("the conflict pane declares a pool region");
    let theme = Arc::new(h.stoat.theme.clone());
    let fallback = theme.get(crate::theme::scope::UI_TEXT);
    let (snapshot, mut state) = {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("center editor");
        (
            editor.display_map.snapshot(),
            editor.conflict_view.clone().expect("conflict view state"),
        )
    };

    let area = Rect::new(0, 0, region.width, region.height);
    let mut expected = crate::smooth_scroll::page_buffer(area, &theme);
    let (doc, derived) = Arc::make_mut(&mut state).derived(&snapshot);
    render_conflict_rows(
        &snapshot,
        doc,
        derived,
        0,
        area,
        fallback,
        &theme,
        &mut expected,
        None,
        None,
    );
    let expected = crate::render::serialize_buffer(&expected);

    let mut fills = Vec::new();
    while let Ok(bytes) = rx.try_recv() {
        fills.push(bytes);
    }
    assert!(
        fills
            .iter()
            .any(|batch| batch.windows(expected.len()).any(|w| w == expected)),
        "a pooled fill carries the live three-column body, not the bare center buffer",
    );
}

#[test]
fn emit_smooth_scroll_pushes_pool_region_then_scroll() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/pool");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    // Lay the panes out so the focused editor has a non-zero rect.
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    emit_smooth_scroll(&mut h.stoat);

    let bytes = rx.try_recv().expect("an APC batch was pushed");
    let cmds = command::decode_stream(&bytes);
    assert!(
        matches!(cmds.first(), Some(Command::PoolRegion(_))),
        "first frame should declare the pool region, got {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| matches!(c, Command::Scroll(_))),
        "the scroll target eases into the region behind it, got {cmds:?}"
    );

    // A resize hands the pool a rectangle its next event replaces, so a
    // page rendered into this one is never composited. Changing the height
    // alone leaves what the pane paints untouched, so nothing but the
    // geometry moved.
    while rx.try_recv().is_ok() {}
    let size = h.stoat.size();
    h.resize(size.width, size.height - 2);
    emit_smooth_scroll(&mut h.stoat);

    let mut resized = Vec::new();
    while let Ok(batch) = rx.try_recv() {
        resized.extend(command::decode_stream(&batch));
    }
    assert!(
        resized.iter().any(|c| matches!(c, Command::PoolRegion(_))),
        "the resize re-declares the region, got {resized:?}"
    );
    assert!(
        !resized.iter().any(|c| matches!(c, Command::Fill(_))),
        "and ships no page into a rectangle still moving, got {resized:?}"
    );
    assert!(
        h.stoat.pool_settle.is_some(),
        "with a settle frame armed to fill once it stops"
    );
}

/// The pooled pages hold rendered text, so anything that recolors every row
/// has to reach their content hash or the pool glides the old colors.
///
/// `change` is the recolor under test, applied between two scrolls: the
/// first proves an untouched surface re-enters nothing, and the second
/// proves the change reaches the hash.
fn pool_reenters_after(change: impl FnOnce(&mut Stoat)) {
    use stoatty_protocol::command::{Command, FillCommand};
    use tokio::sync::mpsc::UnboundedReceiver;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/diff-syntax-pool");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .set_diff_view(true);

    fn drain_fills(rx: &mut UnboundedReceiver<Vec<u8>>) -> Vec<u64> {
        let mut filled = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            for cmd in command::decode_stream(&batch) {
                if let Command::Fill(FillCommand { index, .. }) = cmd {
                    filled.push(index);
                }
            }
        }
        filled
    }

    let scroll = |stoat: &mut Stoat, offset: f32| {
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        editor.scroll_offset = offset;
        editor.scroll_glide = ScrollGlide::Page;
    };

    emit_smooth_scroll(&mut h.stoat);
    let _ = drain_fills(&mut rx);

    scroll(&mut h.stoat, 0.5);
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        drain_fills(&mut rx).is_empty(),
        "a same-window scroll with nothing changed re-enters nothing"
    );

    change(&mut h.stoat);
    scroll(&mut h.stoat, 0.9);
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        !drain_fills(&mut rx).is_empty(),
        "the change recolors the page, so the pool re-enters it"
    );
}

#[test]
fn a_diff_syntax_toggle_reenters_the_pool_pages() {
    pool_reenters_after(|stoat| stoat.diff_syntax = false);
}

/// The tint dial recolors every changed char, so a step has to re-enter the
/// buffered pages rather than let them glide the colors from before it.
#[test]
fn a_diff_tint_step_reenters_the_pool_pages() {
    pool_reenters_after(|stoat| stoat.diff_tint = 2);
}

#[test]
fn emit_after_edit_reenters_pool_pages() {
    use stoatty_protocol::command::{Command, FillCommand};
    use tokio::sync::mpsc::UnboundedReceiver;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    // A file outside any git root keeps diff_version at 0, so only the
    // display snapshot version can carry an edit into the page content hash.
    let root = PathBuf::from("/pool");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    fn drain_fills(rx: &mut UnboundedReceiver<Vec<u8>>) -> Vec<u64> {
        let mut filled = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            for cmd in command::decode_stream(&batch) {
                if let Command::Fill(FillCommand { index, .. }) = cmd {
                    filled.push(index);
                }
            }
        }
        filled
    }

    emit_smooth_scroll(&mut h.stoat);
    assert!(
        !drain_fills(&mut rx).is_empty(),
        "the first emit prefills the pool window"
    );

    // The editor pool refills only while its scroll target moves. A sub-page
    // glide keeps the same page window, so a bare scroll re-enters nothing.
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.scroll_offset = 0.5;
        editor.scroll_glide = ScrollGlide::Page;
    }
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        drain_fills(&mut rx).is_empty(),
        "a same-window scroll with no content change re-enters no pages"
    );

    h.edit_focused(0..0, "x");
    let _ = drain_fills(&mut rx);

    // The edit bumps the snapshot version, so the next moving emit wipes the
    // buffered window and re-enters its pages rather than compositing stale
    // pre-edit text.
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.scroll_offset = 0.9;
        editor.scroll_glide = ScrollGlide::Page;
    }
    emit_smooth_scroll(&mut h.stoat);
    assert!(
        !drain_fills(&mut rx).is_empty(),
        "a scroll after an edit re-enters the pool's pages with fresh text"
    );
}

#[test]
fn emit_smooth_scroll_glide_uses_the_eased_offset() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/pool");
    let path = root.join("a.txt");
    let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    // Simulate a page glide. scroll_row jumped to a distant row while
    // scroll_offset still lags near the top. The emit must carry the lagging
    // offset (its page is 0), not the target row's page, so the pool eases
    // up to it.
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.scroll_row = 50;
        editor.scroll_offset = 1.0;
        editor.scroll_glide = ScrollGlide::Page;
    }

    emit_smooth_scroll(&mut h.stoat);

    let bytes = rx.try_recv().expect("an APC batch was pushed");
    let cmds = command::decode_stream(&bytes);
    let scroll = cmds
        .iter()
        .find_map(|c| match c {
            Command::Scroll(s) => Some(*s),
            _ => None,
        })
        .expect("a scroll command");
    assert_eq!(
        scroll.page, 0,
        "a glide emits the eased offset's page (1.0 -> page 0), not scroll_row 50's page"
    );
}

#[test]
fn emit_smooth_scroll_anchors_the_cursor_during_a_wheel_glide() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/pool");
    let path = root.join("a.txt");
    let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    // Move the cursor off the first line so the anchored row is non-trivial,
    // then drop the batches those moves pushed.
    for _ in 0..15 {
        action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveDown);
    }
    while rx.try_recv().is_ok() {}

    // Collect every batch the emit and its async page fills push. An idle
    // pane (no glide) ships no cursor anchor.
    emit_smooth_scroll(&mut h.stoat);
    let mut idle = Vec::new();
    while let Ok(bytes) = rx.try_recv() {
        idle.extend(command::decode_stream(&bytes));
    }
    assert!(
        !idle.iter().any(|c| matches!(c, Command::PoolCursor(_))),
        "an idle emit carries no cursor anchor, got {idle:?}"
    );

    // Arm a wheel glide with a known on-screen cursor cell.
    let expected_row = {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.scroll_glide = ScrollGlide::Wheel;
        editor.cursor_screen_cell = Some((7, 3));
        view::cursor_display_row(editor) as u64
    };
    assert_eq!(expected_row, 15, "the cursor sits on display row 15");

    emit_smooth_scroll(&mut h.stoat);
    let mut cmds = Vec::new();
    while let Ok(bytes) = rx.try_recv() {
        cmds.extend(command::decode_stream(&bytes));
    }
    let anchor = cmds
        .iter()
        .find_map(|c| match c {
            Command::PoolCursor(p) => Some(*p),
            _ => None,
        })
        .expect("a pool_cursor frame while gliding");
    assert_eq!(
        (anchor.row, anchor.col),
        (15, 7),
        "the anchor carries the cursor's display row and recorded column"
    );
}

#[test]
fn a_jump_ships_the_cursor_anchor_at_the_landed_row() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/pool");
    let path = root.join("a.txt");
    let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);
    while rx.try_recv().is_ok() {}

    // Record line 80 in the jumplist, then strand the view back at the top
    // so returning to that entry is a long jump down.
    let line_80 = body.find("line 80\n").expect("line 80 exists");
    movement::jump_to_offset(&mut h.stoat, line_80);
    action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    movement::jump_to_offset(&mut h.stoat, 0);
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);
        editor.cursor_screen_cell = Some((4, 2));
        view::ensure_cursor_in_view(editor, 3);
        editor.scroll_glide = ScrollGlide::None;
    }

    action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        assert_eq!(
            editor.scroll_glide,
            ScrollGlide::Page,
            "the jump back arms the glide the anchor emit gates on"
        );
    }

    emit_smooth_scroll(&mut h.stoat);
    let mut cmds = Vec::new();
    while let Ok(bytes) = rx.try_recv() {
        cmds.extend(command::decode_stream(&bytes));
    }
    let anchor = cmds
        .iter()
        .find_map(|c| match c {
            Command::PoolCursor(p) => Some(*p),
            _ => None,
        })
        .expect("a jump arms a glide, so the emit carries a cursor anchor");
    assert_eq!(
        (anchor.row, anchor.col),
        (80, 4),
        "the anchor carries the landed display row, so the cursor rides the content to it"
    );
}

#[test]
fn wheel_glide_defers_the_relative_line_refill_to_settle() {
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/glide");
    let path = root.join("a.txt");
    let body: String = (0..400).map(|i| format!("line {i}\n")).collect();
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    // Relative numbering is the default. Bake the resting window so the pool
    // holds its pages and pool_current_line records the resting cursor line.
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);
    }
    emit_smooth_scroll(&mut h.stoat);
    h.settle();
    let _ = drain_apc(&mut rx);

    // Arm a wheel glide. wheel_scroll advances the target and drags the
    // cursor's buffer line into the scrolloff band, the drag that would churn
    // the relative-number content version every tick without the held line.
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        view::wheel_scroll(editor, true);
    }
    app::tick_animation(&mut h.stoat, 0.016);
    assert!(app::animating(&h.stoat), "the wheel glide is still easing");

    // Mid-glide the held line keeps the content version stable, so the
    // buffered window does not refill. At most one edge page enters.
    emit_smooth_scroll(&mut h.stoat);
    h.settle();
    let mid = drain_apc(&mut rx);
    let mid_fills = mid.iter().filter(|c| matches!(c, Command::Fill(_))).count();
    assert!(
        mid_fills <= 1,
        "a held-line glide refills at most one edge page, got {mid_fills}: {mid:?}"
    );

    // Settling the glide releases the held line. The fresh cursor line bumps
    // the content version once, refilling the whole window to match the grid.
    for _ in 0..1000 {
        if !app::animating(&h.stoat) {
            break;
        }
        app::tick_animation(&mut h.stoat, 0.016);
    }
    emit_smooth_scroll(&mut h.stoat);
    h.settle();
    let settled = drain_apc(&mut rx);
    let settled_fills = settled
        .iter()
        .filter(|c| matches!(c, Command::Fill(_)))
        .count();
    assert!(
        settled_fills > 1,
        "the settle emit refills the whole window once, got {settled_fills}: {settled:?}"
    );
}

/// Buffer the pool window of a 400-line file for a terminal at `protocol`,
/// with the rich gutter on, then move the cursor and the scroll row one line
/// down and report the pages each kind of fill in that move named.
fn relative_line_scroll_fills(protocol: u32) -> (Vec<u64>, Vec<u64>) {
    use stoatty_protocol::command::{Command, FillCommand};

    let mut h = Stoat::test();
    h.stoat.theme = Arc::new(rgb_diagnostic_theme());
    h.stoat.stoatty_protocol = protocol;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/relative");
    let path = root.join("a.txt");
    let body: String = (0..400).map(|i| format!("line {i}\n")).collect();
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);
    emit_smooth_scroll(&mut h.stoat);
    h.settle();
    let _ = drain_apc(&mut rx);

    // Moved without a glide, since a glide holds the numbered line still.
    let line_one = body.find("line 1\n").expect("line 1 exists");
    h.stoat.collapse_focused_cursor_to(line_one);
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.scroll_row = 1;
        editor.scroll_glide = ScrollGlide::None;
    }
    emit_smooth_scroll(&mut h.stoat);
    h.settle();

    let (mut fills, mut decorations) = (Vec::new(), Vec::new());
    for command in drain_apc(&mut rx) {
        match command {
            Command::Fill(FillCommand { index, .. }) => fills.push(index),
            Command::FillDecorations(FillCommand { index, .. }) => decorations.push(index),
            _ => {},
        }
    }
    (fills, decorations)
}

#[test]
fn a_relative_line_scroll_redecorates_the_window_instead_of_refilling_it() {
    assert_eq!(
        relative_line_scroll_fills(stoatty_protocol::PROTOCOL_VERSION),
        (Vec::new(), vec![0, 1, 2, 3, 4]),
        "the buffered pages keep their cells and take the new gutter runs"
    );
}

/// A page's spotlight version covers only the part of the span on its own
/// rows. A move refills only the pages whose lit cells change, also when a
/// column moves on the first or last row of a page.
#[test]
fn a_spotlight_page_version_covers_only_its_own_rows() {
    // Spans run [start row, start column, end row, end column] over 20-row
    // pages, so row 19 ends page 0 and row 20 starts page 1.
    let version = |span: [u32; 4], page: u64| {
        let [start_row, start_column, end_row, end_column] = span;
        let points = (
            DisplayPoint::new(start_row, start_column),
            DisplayPoint::new(end_row, end_column),
        );
        spotlight_page_version(Some(([1, 2, 3], points)), page, 20)
    };
    let refilled = |from: [u32; 4], to: [u32; 4]| -> Vec<u64> {
        (0..3)
            .filter(|&page| version(from, page) != version(to, page))
            .collect()
    };

    for (from, to, pages, label) in [
        ([5, 2, 30, 7], [5, 2, 35, 7], vec![1], "end in page 1"),
        ([5, 2, 30, 7], [8, 2, 30, 7], vec![0], "start in page 0"),
        ([20, 2, 30, 7], [20, 5, 30, 7], vec![1], "start on row 20"),
        ([5, 2, 20, 7], [5, 2, 20, 9], vec![1], "end on row 20"),
        ([5, 2, 19, 7], [5, 2, 19, 9], vec![0], "end on row 19"),
        ([1, 0, 1, 9], [50, 0, 50, 9], vec![0, 2], "span on row 1"),
    ] {
        assert_eq!(refilled(from, to), pages, "the {label} moves");
    }
    let span = (DisplayPoint::new(5, 2), DisplayPoint::new(30, 7));
    assert_ne!(
        spotlight_page_version(Some(([1, 2, 3], span)), 0, 20),
        spotlight_page_version(Some(([4, 5, 6], span)), 0, 20),
        "the same span in another color refills its page",
    );
    assert_eq!(
        (
            version([5, 2, 30, 7], 2),
            spotlight_page_version(None, 0, 20)
        ),
        (0, 0),
        "(a page the span misses, a page with no spotlight)",
    );
}

/// Rest the focused editor of a 400-line file with its top row on `start`,
/// then jump the cursor to `landing` and run the page glide frame by frame.
///
/// Reports the page under the rest's top row, the pages the glide's frames
/// redecorated, and the pages its settle redecorated. The rich gutter is
/// on, so the jump's new relative-number line reaches the held pages as
/// decorations-only fills.
fn page_glide_redecorations(start: usize, landing: usize) -> (u64, Vec<u64>, Vec<u64>) {
    use stoatty_protocol::command::{Command, FillCommand, PoolRegionCommand, NON_PANE_POOL_BASE};

    let mut h = Stoat::test();
    h.stoat.theme = Arc::new(rgb_diagnostic_theme());
    h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/landed");
    let path = root.join("a.txt");
    let body: String = (0..400).map(|i| format!("line {i}\n")).collect();
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    let size = h.stoat.size();
    h.stoat.active_workspace_mut().layout(size);

    let line = |n: usize| body.find(&format!("line {n}\n")).expect("the line exists");
    h.stoat.collapse_focused_cursor_to(line(start));
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .scroll_row = start as u32;
    emit_smooth_scroll(&mut h.stoat);
    h.settle();
    let height = drain_apc(&mut rx)
        .into_iter()
        .find_map(|command| match command {
            Command::PoolRegion(PoolRegionCommand { pool, height, .. })
                if pool < NON_PANE_POOL_BASE =>
            {
                Some(u64::from(height))
            },
            _ => None,
        })
        .expect("the editor declares its pool");

    // Two 10-row viewports down, inside the three a glide eases across
    // rather than snaps.
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .viewport_rows = Some(10);
    h.stoat.collapse_focused_cursor_to(line(landing));
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        assert!(view::follow_jump(editor, 3), "the jump moves the view");
        assert_eq!(editor.scroll_glide, ScrollGlide::Page);
    }

    let redecorated = |commands: Vec<Command>| -> Vec<u64> {
        commands
            .into_iter()
            .filter_map(|command| match command {
                Command::FillDecorations(FillCommand { pool, index, .. })
                    if pool < NON_PANE_POOL_BASE =>
                {
                    Some(index)
                },
                _ => None,
            })
            .collect()
    };
    // Every frame of the glide emits, as the render loop does, so the window
    // follows the glide and the pages it passes fill on the way.
    emit_smooth_scroll(&mut h.stoat);
    h.settle();
    let mut gliding = redecorated(drain_apc(&mut rx));
    for _ in 0..1000 {
        app::tick_animation(&mut h.stoat, 0.016);
        if !app::animating(&h.stoat) {
            break;
        }
        emit_smooth_scroll(&mut h.stoat);
        h.settle();
        gliding.extend(redecorated(drain_apc(&mut rx)));
    }
    assert!(!app::animating(&h.stoat), "the glide settles");

    // The first emit after the glide is the settle.
    emit_smooth_scroll(&mut h.stoat);
    h.settle();
    (
        start as u64 / height,
        gliding,
        redecorated(drain_apc(&mut rx)),
    )
}

/// A jump lands the cursor before it arms the page glide, so the pages the
/// glide fills already carry the landed line. The held pages take the new
/// gutter runs as the glide starts, and the settle has none to refresh.
#[test]
fn a_page_glide_fills_with_the_landed_line() {
    assert_eq!(
        page_glide_redecorations(0, 20),
        (0, vec![0, 1, 2, 3, 4], Vec::new()),
        "(page, glide redecorations, settle redecorations) from the top",
    );
}

/// A glide down never composites the held pages above its start, so they
/// keep their old gutter runs until a move heads back up to them.
#[test]
fn a_page_glide_redecorates_no_page_above_its_start() {
    let (page, gliding, settle) = page_glide_redecorations(200, 220);
    assert_eq!(
        (gliding, settle),
        ((page..page + 3).collect(), Vec::new()),
        "(glide redecorations, settle redecorations) from page {page}",
    );
}

#[test]
fn an_older_terminal_still_gets_whole_refills() {
    assert_eq!(
        relative_line_scroll_fills(4),
        (vec![0, 1, 2, 3, 4], Vec::new()),
        "a terminal without decorations-only fills repaints the window"
    );
}

#[test]
fn error_popout_emits_scaled_runs_under_stoatty() {
    use crate::render::TEXT_SCALE_COMPACT;
    use lsp_types::MessageType;
    use stoatty_protocol::command::Command;

    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    let msg = "rust-analyzer failed to load the workspace: Cargo.toml is malformed and could not be parsed, so diagnostics are unavailable";
    h.stoat.lsp_message = Some((MessageType::ERROR, msg.to_string()));

    let buf = h.stoat.render();
    emit_apc_scene(&mut h.stoat);

    let mut raw = Vec::new();
    while let Ok(batch) = rx.try_recv() {
        raw.extend(batch);
    }
    let scene = String::from_utf8_lossy(&raw);
    let cmds = command::decode_stream(&raw);

    // A run's text streams between its open and close markers, so it reaches
    // the terminal through the APC scene rather than the cell grid.
    assert!(
        scene.contains("rust-analyzer"),
        "the error head streams into the APC scene"
    );
    assert!(
        scene.contains("diagnostics"),
        "the wrapped tail streams into the APC scene"
    );

    // The popout's bottom line paints directly above the bar at the bar's
    // compact scale.
    let popout_bottom = (buf.area.height - 2) as i16 * 16;
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::TextRun(t) if t.scale == TEXT_SCALE_COMPACT && t.row == popout_bottom
        )),
        "the popout paints as a compact-scale run above the bar"
    );

    // The rich arm keeps the text off the cell grid, unlike the fallback.
    let in_cells = (0..buf.area.height).any(|y| {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        row.contains("rust-analyzer")
    });
    assert!(!in_cells, "no grid cell carries the error text");
}

#[test]
fn transient_status_message_keeps_the_rich_status_bar() {
    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.stoat.set_status("Current working directory is now /w");

    let buf = h.stoat.render();
    emit_apc_scene(&mut h.stoat);

    let mut raw = Vec::new();
    while let Ok(batch) = rx.try_recv() {
        raw.extend(batch);
    }
    let scene = String::from_utf8_lossy(&raw);

    assert!(
        scene.contains("Current working directory"),
        "the status message streams into the APC scene"
    );
    let in_cells = (0..buf.area.height).any(|y| {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        row.contains("Current working directory")
    });
    assert!(!in_cells, "no grid cell carries the status message");
}

#[test]
fn quitting_hands_the_terminal_its_own_defaults_back() {
    let mut h = Stoat::test();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);

    emit_reset_default_colors(&h.stoat);

    let sent: Vec<u8> = std::iter::from_fn(|| rx.try_recv().ok())
        .flatten()
        .collect();
    assert_eq!(
        sent,
        b"\x1b]110\x1b\\\x1b]111\x1b\\".to_vec(),
        "OSC 110 and 111 undo the defaults the session overrode",
    );
}
