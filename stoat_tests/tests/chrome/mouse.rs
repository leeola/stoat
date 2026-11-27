use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{layout::Rect, style::Style};
use slotmap::SlotMap;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use stoat::{
    action_handlers,
    app::{Stoat, UpdateEffect, PREVIEW_WHEEL_ROWS, WHEEL_BINDING_COOLDOWN},
    diff_changes,
    diff_changes::DiffChangesState,
    editor_state::{EditorId, EditorState},
    host::NoopLsp,
    keymap_state::active_modal,
    merge_view::Side,
    mouse::{
        boxed_modal_surfaces, editor_screen_to_offset, focus_at, handle_hover, handle_mouse_scroll,
        translate_mouse_to_focused, MinimapDrag,
    },
    pane::{FocusTarget, View},
    rebase::RebasePause,
    render::{
        conflict_view::{ConflictHover, ConflictLayout},
        hover::HoverPopup,
    },
    review_walk::{ReturnRef, ReviewWalk},
    run::CmdId,
    test_fixture::{mouse_event, open_scratch_file},
    workspace::Workspace,
};
use stoat_action::{
    Action, ActionKind, Conflict, ConflictNextFile, ConflictPickOurs, ConflictPickTheirs, MoveDown,
    NewTab, OpenFile, SplitDown, SplitRight, TogglePaneWiden,
};
use stoat_config::MinimapMode;
use stoat_text::Point;
use stoatty_protocol::{
    command,
    command::Command,
    window_ipc::{MouseButton as IpcMouseButton, MouseKind, WindowIpcEvent},
    NUDGE_PROTOCOL,
};

/// A buffer with `count` single-line diagnostics on consecutive rows,
/// painted once so the pane areas and the render's span cache exist.
fn hover_diagnostics_harness(count: u32) -> (stoat::test_harness::TestHarness, EditorId) {
    let mut h = Stoat::test();
    let root = PathBuf::from("/diag-hover");
    let path = root.join("a.txt");
    h.fake_fs()
        .insert_file(&path, b"alpha bravo\ncharlie delta\necho foxtrot\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();

    set_hover_diagnostics(&mut h, &path, count);
    let _ = h.stoat.render();
    let (editor_id, _) = h.stoat.focused_editor_ids().expect("a focused editor");
    (h, editor_id)
}

fn set_hover_diagnostics(h: &mut stoat::test_harness::TestHarness, path: &Path, count: u32) {
    let diagnostics = (0..count)
        .map(|line| lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position { line, character: 0 },
                end: lsp_types::Position { line, character: 5 },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            ..Default::default()
        })
        .collect();
    h.seed_diagnostics(path, diagnostics);
}

fn cached_span_count(h: &stoat::test_harness::TestHarness, editor_id: EditorId) -> usize {
    h.stoat
        .active_workspace()
        .editors
        .get(editor_id)
        .expect("editor exists")
        .diagnostic_span_cache
        .as_ref()
        .expect("a painted editor has a span cache")
        .spans
        .len()
}

// TEST IMPORTS

/// A harness over a ten-line buffer whose keymap binds `src`, with the
/// wheel cooldown clear so the first notch dispatches.
fn wheel_binding_harness(src: &str) -> stoat::test_harness::TestHarness {
    let mut h = Stoat::test();
    open_scratch_file(&mut h, "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n");
    let (config, errors) = stoat_config::parse(src);
    assert!(errors.is_empty(), "parse errors: {errors:?}");
    h.stoat.keymap = stoat::keymap::Keymap::compile(&config.expect("config"));
    h.stoat.wheel_binding_last = None;
    h
}

fn wheel(kind: MouseEventKind, modifiers: KeyModifiers) -> MouseEvent {
    MouseEvent {
        kind,
        column: 0,
        row: 0,
        modifiers,
    }
}

#[test]
fn a_modified_wheel_runs_its_binding_on_the_focused_editor() {
    let mut h = wheel_binding_harness("on key { mode == normal { Alt-WheelDown -> MoveDown(); } }");
    let before = h.cursor_display_positions();

    handle_mouse_scroll(
        &mut h.stoat,
        wheel(MouseEventKind::ScrollDown, KeyModifiers::ALT),
        1.0,
    );

    assert_eq!(
        (before, h.cursor_display_positions()),
        (vec![(0, 0)], vec![(1, 0)]),
        "the bound notch moves the focused editor's cursor down a row"
    );
}

#[test]
fn a_second_wheel_notch_inside_the_cooldown_does_not_dispatch() {
    let mut h = wheel_binding_harness("on key { mode == normal { Alt-WheelDown -> MoveDown(); } }");

    for _ in 0..2 {
        handle_mouse_scroll(
            &mut h.stoat,
            wheel(MouseEventKind::ScrollDown, KeyModifiers::ALT),
            1.0,
        );
    }

    assert_eq!(
        h.cursor_display_positions(),
        vec![(1, 0)],
        "a trackpad burst walks one jump, not one per report"
    );
}

#[test]
fn a_notch_past_the_cooldown_dispatches_again() {
    let mut h = wheel_binding_harness("on key { mode == normal { Alt-WheelDown -> MoveDown(); } }");

    handle_mouse_scroll(
        &mut h.stoat,
        wheel(MouseEventKind::ScrollDown, KeyModifiers::ALT),
        1.0,
    );
    // Backdate the gate rather than sleeping, so the test stays pure.
    h.stoat.wheel_binding_last = h
        .stoat
        .wheel_binding_last
        .map(|t| t - WHEEL_BINDING_COOLDOWN);
    handle_mouse_scroll(
        &mut h.stoat,
        wheel(MouseEventKind::ScrollDown, KeyModifiers::ALT),
        1.0,
    );

    assert_eq!(
        h.cursor_display_positions(),
        vec![(2, 0)],
        "a deliberate second notch lands once the cooldown has passed"
    );
}

#[test]
fn an_unbound_modifier_and_a_bare_notch_both_fall_through() {
    // The bare arm is bound too, so a fall-through is the early return on
    // an empty modifier set rather than a lookup miss.
    let mut h = wheel_binding_harness(
        "on key { mode == normal { Alt-WheelDown -> MoveDown(); WheelDown -> MoveDown(); } }",
    );

    handle_mouse_scroll(
        &mut h.stoat,
        wheel(MouseEventKind::ScrollDown, KeyModifiers::CONTROL),
        1.0,
    );
    let after_unbound = h.cursor_display_positions();
    handle_mouse_scroll(
        &mut h.stoat,
        wheel(MouseEventKind::ScrollDown, KeyModifiers::NONE),
        1.0,
    );

    assert_eq!(
        (after_unbound, h.cursor_display_positions()),
        (vec![(0, 0)], vec![(0, 0)]),
        "an unbound modifier and an unmodified notch both scroll instead"
    );
}

#[test]
fn mouse_translates_to_focused_pane_coords() {
    let mut h = Stoat::test();
    let pane_id = h.stoat.active_workspace().panes.focus();
    h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(10, 5, 20, 8);
    let translated = translate_mouse_to_focused(&h.stoat, 15, 9);
    assert_eq!(translated, Some((5, 4)));
}

#[test]
fn mouse_above_focused_pane_saturates_to_zero() {
    let mut h = Stoat::test();
    let pane_id = h.stoat.active_workspace().panes.focus();
    h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(10, 5, 20, 8);
    let translated = translate_mouse_to_focused(&h.stoat, 3, 2);
    assert_eq!(translated, Some((0, 0)));
}

#[test]
fn hover_popup_overflows_across_a_vertical_split() {
    use ratatui::style::Style;
    use stoat::{render::hover::HoverPopup, test_harness::TestHarness};

    let mut h = TestHarness::with_size(80, 24);
    let root = PathBuf::from("/hover");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let left = h.stoat.active_workspace().panes.focus();
    action_handlers::dispatch(&mut h.stoat, &SplitRight);
    h.stoat
        .active_workspace_mut()
        .panes
        .resize(Rect::new(0, 0, 80, 24));
    let left_content = h.stoat.active_workspace().panes.pane(left).area;
    focus_at(&mut h.stoat, left_content.x + 1, left_content.y + 1);
    h.stoat.render();
    let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;

    // A hover wider than the left pane.
    h.stoat.pending_hover = Some(HoverPopup::new(
        vec![vec![("x".repeat(60), Style::default())]],
        0,
        editor_id,
    ));

    let (popup, _) = stoat::render::hover::hover_popup_layout(&mut h.stoat).expect("layout");
    assert!(
        popup.width > left_content.width,
        "the popup widens past the left pane ({} > {})",
        popup.width,
        left_content.width,
    );
    assert!(
        popup.x + popup.width > left_content.x + left_content.width,
        "the popup crosses the divider into the right pane"
    );
}

#[test]
fn closing_a_mouse_focused_pane_leaves_focus_readers_panic_free() {
    // Mouse-focusing a split pane then closing it used to leave that pane's
    // id dangling in `ws.focus`, so the next focus-reading event dereferenced
    // a freed SlotMap key and panicked. `FocusTarget::SplitPane` is now a unit
    // variant resolved through the live pane-tree focus.
    use stoat::test_harness::TestHarness;

    let mut h = TestHarness::with_size(80, 24);
    let root = PathBuf::from("/close");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\n");
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let right = {
        let ws = h.stoat.active_workspace_mut();
        let right = ws.panes.split(stoat::pane::Axis::Vertical);
        ws.panes.resize(Rect::new(0, 0, 80, 24));
        right
    };
    let right_area = h.stoat.active_workspace().panes.pane(right).area;
    focus_at(&mut h.stoat, right_area.x + 1, right_area.y + 1);
    assert_eq!(
        h.stoat.active_workspace().panes.focus(),
        right,
        "the mouse click focused the right pane"
    );

    action_handlers::dispatch(&mut h.stoat, &stoat_action::ClosePane);
    assert!(
        !h.stoat
            .active_workspace()
            .panes
            .split_pane_ids()
            .contains(&right),
        "the closed pane is gone"
    );

    // Each of these resolves the focused pane. Before the fix they panicked
    // on the stale id left behind in `ws.focus`.
    assert!(translate_mouse_to_focused(&h.stoat, 1, 1).is_some());
    focus_at(&mut h.stoat, 1, 1);
    let ws = h.stoat.active_workspace();
    assert!(
        ws.panes.contains(ws.panes.focus()),
        "focus resolves to a live pane after the close"
    );
}

#[test]
fn hover_popup_overflows_into_the_pane_below() {
    use ratatui::style::Style;
    use stoat::{render::hover::HoverPopup, test_harness::TestHarness};

    let mut h = TestHarness::with_size(40, 24);
    let root = PathBuf::from("/hover");
    let path = root.join("a.txt");
    let content: String = (0..40).map(|_| "x\n").collect();
    h.fake_fs().insert_file(&path, content.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let top = h.stoat.active_workspace().panes.focus();
    action_handlers::dispatch(&mut h.stoat, &SplitDown);
    h.stoat
        .active_workspace_mut()
        .panes
        .resize(Rect::new(0, 0, 40, 24));
    let top_content = h.stoat.active_workspace().panes.pane(top).area;
    focus_at(&mut h.stoat, top_content.x + 1, top_content.y + 1);
    h.stoat.render();
    let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;

    // Anchor on the top pane's last visible row (each "x\n" line is 2 bytes).
    let last_row_line = top_content.height as usize - 1;
    h.stoat.pending_hover = Some(HoverPopup::new(
        vec![vec![("hi".to_string(), Style::default())]],
        last_row_line * 2,
        editor_id,
    ));

    let (popup, _) = stoat::render::hover::hover_popup_layout(&mut h.stoat).expect("layout");
    let cursor_row = top_content.y + last_row_line as u16;
    assert!(
        popup.y > cursor_row,
        "the popup places below the cursor ({} > {}) instead of flipping above",
        popup.y,
        cursor_row,
    );
    assert!(
        popup.y >= top_content.y + top_content.height,
        "the popup overflows into the pane below"
    );
}

#[test]
fn mouse_routes_to_focused_dock_when_focus_is_dock() {
    use stoat::pane::{DockPanel, DockSide, DockVisibility, View};
    let mut h = Stoat::test();
    let dock_id = h.stoat.active_workspace_mut().docks.insert(DockPanel {
        view: View::Label("dock".into()),
        side: DockSide::Right,
        visibility: DockVisibility::Open { width: 30 },
        default_width: 30,
        area: Rect::new(50, 0, 30, 24),
    });
    h.stoat.active_workspace_mut().focus = FocusTarget::Dock(dock_id);
    let translated = translate_mouse_to_focused(&h.stoat, 60, 7);
    assert_eq!(translated, Some((10, 7)));
}

#[test]
fn mouse_returns_none_when_focused_dock_missing() {
    use stoat::pane::DockId;
    let mut h = Stoat::test();
    let dangling = DockId::default();
    h.stoat.active_workspace_mut().focus = FocusTarget::Dock(dangling);
    let translated = translate_mouse_to_focused(&h.stoat, 10, 10);
    assert_eq!(translated, None);
}

/// Mouse motion used to resolve every diagnostic in the file per event,
/// ahead of the changed-dedupe. It now answers from the version-keyed cache
/// the paint reads, so a sweep resolves the set once and the paint that
/// follows finds it already built.
#[test]
fn hovering_rebuilds_the_shared_diagnostic_span_cache() {
    let (mut h, editor_id) = hover_diagnostics_harness(1);
    assert_eq!(
        cached_span_count(&h, editor_id),
        1,
        "the paint resolved the file's one diagnostic",
    );

    // A second diagnostic moves the set version, leaving the cache stale.
    set_hover_diagnostics(&mut h, &PathBuf::from("/diag-hover/a.txt"), 2);
    handle_hover(&mut h.stoat, 8, 0, KeyModifiers::NONE);

    assert_eq!(
        cached_span_count(&h, editor_id),
        2,
        "the hover refreshed the shared cache rather than resolving its own list",
    );
}

/// Typing in a file full of diagnostics rebuilds nothing.
///
/// The cache holds publish anchors, which already describe where an edit
/// carried each diagnostic, so only the server moving them is a reason to
/// build it again. A cache keyed on the buffer version instead resolved
/// every diagnostic in the file on each keystroke.
#[test]
fn an_edit_leaves_the_span_cache_standing() {
    let (mut h, editor_id) = hover_diagnostics_harness(3);
    let before = cached_spans_ptr(&h, editor_id);

    h.type_keys("i");
    h.type_keys("x");
    let _ = h.stoat.render();

    assert_eq!(
        cached_spans_ptr(&h, editor_id),
        before,
        "the same spans answer for the edited buffer",
    );
}

/// The address of the cached span list, which moves only on a rebuild.
fn cached_spans_ptr(
    h: &stoat::test_harness::TestHarness,
    editor_id: EditorId,
) -> *const stoat::diagnostic_spans::AnchoredDiag {
    h.stoat
        .active_workspace()
        .editors
        .get(editor_id)
        .expect("editor exists")
        .diagnostic_span_cache
        .as_ref()
        .expect("a painted editor has a span cache")
        .spans
        .as_ptr()
}

/// A pointer resting inside one cell reports motion repeatedly. The
/// diagnostic under a cell that has not moved is the one already found, so
/// the repeat answers from it and never reaches the resolve.
#[test]
fn a_repeat_hover_on_the_same_cell_resolves_nothing() {
    let (mut h, editor_id) = hover_diagnostics_harness(1);
    handle_hover(&mut h.stoat, 8, 0, KeyModifiers::NONE);
    assert_eq!(cached_span_count(&h, editor_id), 1);

    set_hover_diagnostics(&mut h, &PathBuf::from("/diag-hover/a.txt"), 3);

    assert_eq!(
        handle_hover(&mut h.stoat, 8, 0, KeyModifiers::NONE),
        UpdateEffect::None,
        "an unmoved pointer reports the diagnostic it already resolved",
    );
    assert_eq!(
        cached_span_count(&h, editor_id),
        1,
        "and leaves the cache for the next paint to refresh",
    );
}

#[test]
fn hovering_the_lsp_badge_rect_opens_and_closes() {
    let mut h = Stoat::test();
    h.stoat.lsp_badge_rect = Some(Rect::new(10, 5, 6, 1));

    assert_eq!(
        handle_hover(&mut h.stoat, 12, 5, KeyModifiers::NONE),
        UpdateEffect::Redraw,
        "hovering onto the badge redraws"
    );
    assert!(h.stoat.lsp_badge_hovered, "hovering sets the flag");

    assert_eq!(
        handle_hover(&mut h.stoat, 11, 5, KeyModifiers::NONE),
        UpdateEffect::None,
        "still inside the badge is a no-op"
    );
    assert!(h.stoat.lsp_badge_hovered);

    assert_eq!(
        handle_hover(&mut h.stoat, 0, 0, KeyModifiers::NONE),
        UpdateEffect::Redraw,
        "moving off the badge redraws"
    );
    assert!(!h.stoat.lsp_badge_hovered, "moving off clears the flag");
}

#[test]
fn badge_disappearing_clears_the_hover_flag() {
    let mut h = Stoat::test();
    h.stoat.lsp_badge_rect = Some(Rect::new(10, 5, 6, 1));
    handle_hover(&mut h.stoat, 12, 5, KeyModifiers::NONE);
    assert!(h.stoat.lsp_badge_hovered);

    // The scratch buffer has no language server, so the next paint stamps no
    // badge rect.
    let _ = h.stoat.render();
    assert!(h.stoat.lsp_badge_rect.is_none(), "no badge painted");
    assert!(
        !h.stoat.lsp_badge_hovered,
        "the hover flag clears when the badge vanishes"
    );
}

#[test]
fn diff_view_click_maps_into_the_right_column() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 10);
    h.stoat.minimap_override = Some(false);
    open_scratch_file(&mut h, "keep\nnew\ntail\n");

    let (editor_id, buffer_id) = {
        let ws = h.stoat.active_workspace();
        let editor_id = match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        (editor_id, ws.editors[editor_id].buffer_id)
    };
    {
        let base = "keep\nold\ntail\n";
        let text = "keep\nnew\ntail\n";
        let dm = stoat::diff_map::DiffMap::from_structural_changes(
            stoat_language::structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
        );
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .diff_map = Some(dm);
    }
    h.stoat.active_workspace_mut().editors[editor_id].set_diff_view(true);
    // The paint records the right column's text rect, which the click maps
    // against, as every click after a frame does.
    h.stoat.render();

    let (area, text_x) = {
        let ws = h.stoat.active_workspace();
        let area = ws.panes.pane(ws.panes.focus()).area;
        let text = ws.editors[editor_id]
            .text_rect
            .expect("the render records the text rect");
        (area, text.x - area.x)
    };
    // Two cells into the right text, row 0 is the third character of the
    // context line "keep".
    assert_eq!(
        editor_screen_to_offset(&mut h.stoat, editor_id, area, text_x + 2, 0),
        Some(2),
        "a click in the right column lands on the buffer character"
    );
    assert_eq!(
        editor_screen_to_offset(&mut h.stoat, editor_id, area, 10, 0),
        Some(0),
        "a click left of the right column clamps to the buffer line start"
    );
}

#[test]
fn conflict_view_click_maps_center_exact_and_sides_to_row_start() {
    let mut h = Stoat::test();
    let git_root = h.stoat.active_workspace().git_root.clone();
    h.fake_git().add_repo(git_root).conflicted_file(
        "f.txt",
        Some("a\nb\nc\n"),
        Some("a\nB\nc\n"),
        Some("a\nX\nc\n"),
    );
    action_handlers::dispatch(&mut h.stoat, &Conflict);
    // The open leaves the file's alignment to a worker, so the center
    // editor reaches the pane a pump later.
    h.settle();

    let editor_id = {
        let ws = h.stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not the conflict editor"),
        }
    };

    // The paint records the center text, which the click maps against. The
    // MergeGrid's column header and File Header take the first two rows, so
    // the "<<<<<<< ours" marker line is the row after the first text row.
    h.stoat.render();
    let (area, text) = {
        let ws = h.stoat.active_workspace();
        let text = ws.editors[editor_id]
            .text_rect
            .expect("the render records the text rect");
        (ws.panes.pane(ws.panes.focus()).area, text)
    };
    let marker = text.y - area.y + 1;
    let center = text.x - area.x;
    let row_start = editor_screen_to_offset(&mut h.stoat, editor_id, area, center, marker);
    assert!(row_start.is_some(), "the center row resolves");

    assert_eq!(
        editor_screen_to_offset(&mut h.stoat, editor_id, area, center + 3, marker),
        row_start.map(|offset| offset + 3),
        "a center-column click maps to the exact clicked cell"
    );
    assert_eq!(
        editor_screen_to_offset(&mut h.stoat, editor_id, area, 3, marker),
        row_start,
        "an ours-column click lands on the aligned center row start"
    );
    assert_eq!(
        editor_screen_to_offset(&mut h.stoat, editor_id, area, area.width - 3, marker),
        row_start,
        "a theirs-column click lands on the aligned center row start"
    );
    assert_eq!(
        (
            text.y - area.y,
            editor_screen_to_offset(&mut h.stoat, editor_id, area, center, 0),
            editor_screen_to_offset(&mut h.stoat, editor_id, area, center, 1),
        ),
        (2, None, None),
        "a click on either header row maps to no line"
    );
}

/// One chunk whose ours and theirs lines replace the base line under `a`.
const ONE_CHUNK: [&str; 3] = ["a\nbase\n", "a\nours\n", "a\ntheirs\n"];

/// A press on the ours side of an open chunk takes ours, as `o` does, and
/// sets no status. The chunk's band starts on the body's second row, under
/// the shared `a`.
#[test]
fn a_press_on_an_open_chunk_s_ours_side_takes_ours() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    conflict_press(&mut h, layout.ours.start + 2, layout.body.y + 1, false);

    assert_eq!(
        (conflict_center(&h), h.stoat.pending_message.clone()),
        ("a\nours\n".to_string(), None),
    );
}

/// A press on the theirs side of an open chunk takes theirs, as `t` does.
#[test]
fn a_press_on_the_theirs_side_takes_theirs() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    conflict_press(&mut h, layout.theirs.start + 2, layout.body.y + 1, false);

    assert_eq!(conflict_center(&h), "a\ntheirs\n");
}

/// An Alt press on a side line takes that line alone, as `T` takes the
/// line at the cursor, and the rest of the chunk stays in its marker block.
/// The chunk then shows a per-row mix, and a second Alt press takes the
/// other theirs line too.
#[test]
fn an_alt_press_on_a_side_line_takes_that_line_alone() {
    let (mut h, layout) =
        conflict_press_harness(["a\nb1\nb2\nc\n", "a\nO1\nO2\nc\n", "a\nT1\nT2\nc\n"]);
    conflict_press(&mut h, layout.theirs.start + 2, layout.body.y + 2, true);
    let second_line = conflict_center(&h);
    h.stoat.render();
    conflict_press(&mut h, layout.theirs.start + 2, layout.body.y + 1, true);

    assert_eq!(
        [second_line, conflict_center(&h)],
        [
            "a\nT2\n<<<<<<< ours\nO1\n=======\nT1\n>>>>>>> theirs\nc\n",
            "a\nT1\nT2\nc\n",
        ],
    );
}

/// A press on the side of a chunk a whole-side pick settled changes no
/// text, and places the cursor at the start of the aligned merged row.
#[test]
fn a_press_on_a_settled_chunk_s_side_moves_the_cursor() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    action_handlers::dispatch(&mut h.stoat, &ConflictPickOurs);
    h.stoat.render();
    h.stoat.collapse_focused_cursor_to(0);
    conflict_press(&mut h, layout.theirs.start + 2, layout.body.y + 1, false);

    assert_eq!(
        (conflict_center(&h), cursor_offset(&mut h)),
        ("a\nours\n".to_string(), Some(2)),
        "the cursor at the start of the ours line, after `a\\n`",
    );
}

/// A press in the merged column of an open chunk places the cursor at the
/// pressed cell and takes no side.
#[test]
fn a_press_in_the_merged_column_places_the_cursor() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    let text_x = action_handlers::focused_editor_mut(&mut h.stoat)
        .and_then(|editor| editor.text_rect)
        .expect("the paint records the merged text")
        .x;
    let marker = conflict_center(&h);
    conflict_press(&mut h, text_x + 3, layout.body.y + 1, false);

    assert_eq!(
        (conflict_center(&h), cursor_offset(&mut h)),
        (marker, Some(5)),
        "the cursor three cells into the `<<<<<<< ours` line, after `a\\n`",
    );
}

/// An Alt press on a band row past a side's lines has no line to take, so
/// it places the cursor at the start of the aligned merged row. The chunk's
/// sides hold one line each, so the band's third row has none.
#[test]
fn an_alt_press_past_a_side_s_lines_places_the_cursor() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    let marker = conflict_center(&h);
    h.stoat.collapse_focused_cursor_to(0);
    conflict_press(&mut h, layout.theirs.start + 2, layout.body.y + 2, true);

    assert_eq!(
        (conflict_center(&h), cursor_offset(&mut h)),
        (marker, Some("a\n<<<<<<< ours\n".len())),
    );
}

/// At an unaligned rest the composite draws a row in the cell its offset
/// rounds to, so a press reads the row drawn under it. Three quarters of a
/// row down, the body's first row shows the chunk's first row.
#[test]
fn a_press_at_an_unaligned_rest_reads_the_row_drawn_there() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("the conflict editor")
        .scroll_frac = 0.75;
    conflict_press(&mut h, layout.ours.start + 2, layout.body.y, false);

    assert_eq!(conflict_center(&h), "a\nours\n");
}

/// Only the left button picks, so a right press on an open chunk's side
/// leaves the chunk's markers.
#[test]
fn a_right_press_on_a_side_takes_nothing() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    let marker = conflict_center(&h);
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Right),
        layout.ours.start + 2,
        layout.body.y + 1,
    ));

    assert_eq!(conflict_center(&h), marker);
}

/// A press's pick is one undo step that puts the cursor back, as a key's
/// pick is.
#[test]
fn a_press_pick_undoes_in_one_step() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    let marker = conflict_center(&h);
    h.stoat.collapse_focused_cursor_to(0);
    conflict_press(&mut h, layout.ours.start + 2, layout.body.y + 1, false);
    h.type_keys("u");

    assert_eq!(
        (conflict_center(&h), cursor_offset(&mut h)),
        (marker, Some(0))
    );
}

/// The ermine_night tints of the ours, theirs, and both pick buttons.
const BUTTON_TINTS: [[u8; 3]; 3] = [[0x1d, 0x2a, 0x1f], [0x2a, 0x22, 0x36], [0x1f, 0x2a, 0x3d]];

/// A press on an open chunk's chip shows its three pick buttons in the
/// ours, theirs, and both tints, and the paint records a button for each
/// pick.
///
/// The buttons stand in the 20 cells that end the merged column. Each takes
/// the cells whose centers it covers, so the cell under the gap after the
/// theirs button takes no pick.
#[test]
fn a_press_on_an_open_chunk_s_chip_offers_three_buttons() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    let (chip, _) = layout.chips[0];
    conflict_press(&mut h, chip.x + 2, chip.y, false);
    h.stoat.render();

    let boxes: Vec<[u8; 3]> = command::decode_stream(&h.stoat.apc_scene.all_bytes())
        .into_iter()
        .filter_map(|command| match command {
            Command::Bar(bar) if bar.radius > 0 && BUTTON_TINTS.contains(&bar.color) => {
                Some(bar.color)
            },
            _ => None,
        })
        .collect();
    let end = layout.merged.end;
    let button = |x: u16, width: u16, kind| (Rect::new(x, chip.y, width, 1), kind);

    assert_eq!(
        (boxes, painted_layout(&mut h).buttons, chip_menu(&h)),
        (
            BUTTON_TINTS.to_vec(),
            vec![
                button(end - 20, 6, ActionKind::ConflictPickOurs),
                button(end - 14, 7, ActionKind::ConflictPickTheirs),
                button(end - 6, 5, ActionKind::ConflictPickBoth),
            ],
            Some(0),
        ),
    );
}

/// A press on the both button takes both sides of the menu's chunk, as `b`
/// does, and closes the menu.
#[test]
fn a_press_on_the_both_button_takes_both_sides() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    let (chip, _) = layout.chips[0];
    conflict_press(&mut h, chip.x + 2, chip.y, false);
    h.stoat.render();
    let both = painted_layout(&mut h)
        .buttons
        .iter()
        .find(|&&(_, kind)| kind == ActionKind::ConflictPickBoth)
        .map(|&(cells, _)| cells)
        .expect("the both button");
    conflict_press(&mut h, both.x, both.y, false);

    assert_eq!(
        (conflict_center(&h), chip_menu(&h)),
        ("a\nours\ntheirs\n".to_string(), None),
    );
}

/// A press on the chip of a chunk that took ours or theirs resets the
/// chunk to its markers, as `x` does.
#[test]
fn a_press_on_a_took_chip_resets_its_chunk() {
    let reset = |took: &dyn Action| {
        let (mut h, _) = conflict_press_harness(ONE_CHUNK);
        let marker = conflict_center(&h);
        action_handlers::dispatch(&mut h.stoat, took);
        h.stoat.render();
        let (chip, _) = painted_layout(&mut h).chips[0];
        conflict_press(&mut h, chip.x + 2, chip.y, false);
        (conflict_center(&h), marker)
    };
    let [(ours, marker), (theirs, _)] = [reset(&ConflictPickOurs), reset(&ConflictPickTheirs)];

    assert_eq!([ours, theirs], [marker.clone(), marker]);
}

/// A key closes the chip's buttons, so the next paint draws the chip again
/// and records no button.
#[test]
fn a_key_closes_the_chip_s_buttons() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    let (chip, _) = layout.chips[0];
    conflict_press(&mut h, chip.x + 2, chip.y, false);
    let opened = chip_menu(&h);
    h.type_keys("j");
    h.stoat.render();

    let view_menu = action_handlers::focused_editor_mut(&mut h.stoat)
        .and_then(|editor| editor.conflict_view.as_ref())
        .and_then(|view| view.chip_menu);
    assert_eq!(
        (
            opened,
            chip_menu(&h),
            view_menu,
            painted_layout(&mut h).buttons
        ),
        (Some(0), None, None, Vec::new()),
    );
}

/// A second press on the chip closes its buttons. The press lands on the
/// chip the last paint recorded, before a repaint draws the buttons there.
#[test]
fn a_second_press_on_the_chip_closes_its_buttons() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    let (chip, _) = layout.chips[0];
    conflict_press(&mut h, chip.x + 2, chip.y, false);
    let opened = chip_menu(&h);
    conflict_press(&mut h, chip.x + 2, chip.y, false);

    assert_eq!((opened, chip_menu(&h)), (Some(0), None));
}

/// A press elsewhere closes the chip's buttons and takes its own path. On
/// the column header that path maps to no line, so the frame still owes a
/// repaint to take the buttons down.
#[test]
fn a_press_elsewhere_closes_the_chip_s_buttons() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    let (chip, _) = layout.chips[0];
    conflict_press(&mut h, chip.x + 2, chip.y, false);
    h.stoat.render();
    let effect = h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        layout.merged.start + 2,
        layout.body.y - 2,
    ));

    assert_eq!((effect, chip_menu(&h)), (UpdateEffect::Redraw, None));
}

/// The pointer over an open chunk's side sets the hover and asks for one
/// repaint. A second move inside the same side asks for none, and a move
/// to the merged column clears the hover and repaints.
#[test]
fn the_pointer_over_an_open_side_sets_the_hover_and_redraws_once() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    let (x, y) = (layout.ours.start + 2, layout.body.y + 1);
    let first = handle_hover(&mut h.stoat, x, y, KeyModifiers::NONE);
    let hovered = h.stoat.conflict_hover;
    let second = handle_hover(&mut h.stoat, x + 1, y, KeyModifiers::NONE);
    let off = handle_hover(&mut h.stoat, layout.merged.start + 2, y, KeyModifiers::NONE);

    assert_eq!(
        (first, hovered, second, off, h.stoat.conflict_hover),
        (
            UpdateEffect::Redraw,
            Some(ConflictHover {
                chunk: 0,
                side: Side::Ours,
                merge_row: None
            }),
            UpdateEffect::None,
            UpdateEffect::Redraw,
            None,
        ),
    );
}

/// With Alt held the hover names the line under the pointer, as an Alt
/// press takes it. A band row past that side's lines holds no line, so it
/// is no hover.
#[test]
fn an_alt_pointer_names_the_line_under_it() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    let x = layout.theirs.start + 2;
    let mut hover = |row: u16| {
        handle_hover(&mut h.stoat, x, layout.body.y + row, KeyModifiers::ALT);
        h.stoat.conflict_hover
    };

    assert_eq!(
        [hover(1), hover(2)],
        [
            Some(ConflictHover {
                chunk: 0,
                side: Side::Theirs,
                merge_row: Some(1)
            }),
            None,
        ],
    );
}

/// The frame hands the hover to the conflict view's paint, which rings the
/// hovered side with one unfilled panel in the scene.
#[test]
fn a_hovered_side_rings_in_the_frame() {
    let (mut h, layout) = conflict_press_harness(ONE_CHUNK);
    handle_hover(
        &mut h.stoat,
        layout.ours.start + 2,
        layout.body.y + 1,
        KeyModifiers::NONE,
    );
    h.stoat.render();

    let rings = command::decode_stream(&h.stoat.apc_scene.all_bytes())
        .into_iter()
        .filter(|command| {
            matches!(command, Command::Panel(panel) if panel.fill.is_none() && panel.corner_radius == 2)
        })
        .count();
    assert_eq!(rings, 1);
}

/// A switch to another file clears the hover, since chunk numbers belong to
/// one file.
#[test]
fn a_file_switch_clears_the_hover() {
    let mut h = Stoat::test();
    let git_root = h.stoat.active_workspace().git_root.clone();
    let mut repo = h.fake_git().add_repo(git_root);
    for file in ["a.txt", "b.txt"] {
        repo.conflicted_file(
            file,
            Some("a\nbase\n"),
            Some("a\nours\n"),
            Some("a\ntheirs\n"),
        );
    }
    action_handlers::dispatch(&mut h.stoat, &Conflict);
    h.settle();
    h.stoat.render();
    let layout = painted_layout(&mut h);
    handle_hover(
        &mut h.stoat,
        layout.ours.start + 2,
        layout.body.y + 1,
        KeyModifiers::NONE,
    );
    let hovered = h.stoat.conflict_hover.is_some();
    action_handlers::dispatch(&mut h.stoat, &ConflictNextFile);
    h.settle();

    assert_eq!((hovered, h.stoat.conflict_hover), (true, None));
}

/// The chunk whose chip shows its pick buttons, as the session holds it.
fn chip_menu(h: &stoat::test_harness::TestHarness) -> Option<usize> {
    h.stoat
        .active_workspace()
        .conflict
        .as_ref()
        .and_then(|session| session.chip_menu)
}

/// The columns, chips, and buttons the conflict editor's last paint
/// recorded.
fn painted_layout(h: &mut stoat::test_harness::TestHarness) -> ConflictLayout {
    action_handlers::focused_editor_mut(&mut h.stoat)
        .and_then(|editor| editor.conflict_layout.clone())
        .expect("the paint records the grid's columns")
}

/// A harness on the conflict view of one file whose ancestor, ours, and
/// theirs stages are `stages`, painted once, and the columns that paint
/// recorded.
fn conflict_press_harness(stages: [&str; 3]) -> (stoat::test_harness::TestHarness, ConflictLayout) {
    let [ancestor, ours, theirs] = stages;
    let mut h = Stoat::test();
    let git_root = h.stoat.active_workspace().git_root.clone();
    h.fake_git().add_repo(git_root).conflicted_file(
        "f.txt",
        Some(ancestor),
        Some(ours),
        Some(theirs),
    );
    action_handlers::dispatch(&mut h.stoat, &Conflict);
    h.settle();
    h.stoat.render();

    let layout = painted_layout(&mut h);
    (h, layout)
}

/// Send a left press at `column`, `row`, with Alt held when `alt` is set.
fn conflict_press(h: &mut stoat::test_harness::TestHarness, column: u16, row: u16, alt: bool) {
    let modifiers = match alt {
        true => KeyModifiers::ALT,
        false => KeyModifiers::NONE,
    };
    h.stoat.update(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers,
    }));
}

/// The byte offset of the focused editor's cursor.
fn cursor_offset(h: &mut stoat::test_harness::TestHarness) -> Option<usize> {
    h.stoat.focused_cursor_pos().map(|(_, offset)| offset)
}

/// The merged text of the conflict view's shown file.
fn conflict_center(h: &stoat::test_harness::TestHarness) -> String {
    let ws = h.stoat.active_workspace();
    let buffer_id = ws
        .conflict
        .as_ref()
        .expect("a session is open")
        .file
        .buffer_id;
    ws.buffers
        .get(buffer_id)
        .expect("the merged buffer")
        .read()
        .expect("buffer poisoned")
        .rope()
        .to_string()
}

#[test]
fn a_press_on_the_status_row_leaves_the_cursor_in_place() {
    let mut h = stoat::test_harness::TestHarness::with_size(80, 24);
    h.seed_focused_buffer(&"line\n".repeat(60));
    h.snapshot();
    let status_y = h.stoat.status_bar_rect().expect("a status row").y;
    let heads = h.head_offsets();

    for button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
        h.stoat
            .update(mouse_event(MouseEventKind::Down(button), 3, status_y));
    }
    assert_eq!(
        (h.head_offsets(), h.stoat.editor_drag),
        (heads, None),
        "no press on the status row reaches the line below the text"
    );

    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 3, 2));
    let pressed = h.head_offsets();
    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        3,
        status_y,
    ));
    assert_eq!(
        h.head_offsets(),
        pressed,
        "a drag onto the status row extends nothing"
    );
}

#[test]
fn a_press_where_a_covered_divider_was_arms_no_drag() {
    let mut h = Stoat::test();
    h.seed_focused_buffer("line\n");
    action_handlers::dispatch(&mut h.stoat, &SplitRight);
    action_handlers::dispatch(&mut h.stoat, &TogglePaneWiden);
    h.snapshot();

    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 2));
    assert_eq!(
        h.stoat.divider_drag, None,
        "column 0 belongs to the widened pane, not to the covered pane's divider"
    );
}

#[test]
fn mouse_focus_change_closes_the_hover_popup() {
    use ratatui::style::Style;
    use stoat::render::hover::HoverPopup;

    let mut h = Stoat::test();
    let ws = h.stoat.active_workspace_mut();
    let left = ws.panes.focus();
    let right = ws.panes.split(stoat::pane::Axis::Vertical);
    ws.panes.resize(Rect::new(0, 0, 101, 24));
    let left_area = h.stoat.active_workspace().panes.pane(left).area;
    let right_area = h.stoat.active_workspace().panes.pane(right).area;

    // Focus the right pane through the real path so ws.focus tracks it, then
    // open a popup there. Its empty area makes any click land outside it.
    focus_at(&mut h.stoat, right_area.x + 1, right_area.y + 1);
    let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
    h.stoat.pending_hover = Some(HoverPopup::new(
        vec![vec![("hi".to_string(), Style::default())]],
        0,
        editor_id,
    ));

    // A left-button Down over the other pane moves focus and closes it.
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        left_area.x + 1,
        left_area.y + 1,
    ));
    assert!(
        h.stoat.pending_hover.is_none(),
        "a mouse focus change closes the hover popup"
    );
}

/// `changes` one-line changes under a focused changes view at 120 by 24,
/// painted once, with the minimap in `mode`.
///
/// Twenty run the document past the pane, and the width leaves room for a
/// per-pane minimap strip.
fn diff_changes_harness(mode: MinimapMode, changes: usize) -> stoat::test_harness::TestHarness {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 24);
    h.stoat.settings.editor_minimap = Some(mode);
    let files: Vec<(String, String, String)> = (0..changes)
        .map(|i| (format!("f{i:02}.txt"), format!("{i}\n"), format!("{i}!\n")))
        .collect();
    let files: Vec<(&str, &str, &str)> = files
        .iter()
        .map(|(path, base, buffer)| (path.as_str(), base.as_str(), buffer.as_str()))
        .collect();
    h.stage_review_scenario("/repo", &files);
    h.open_file(Path::new("/repo/f00.txt"));
    h.cover_with_diff_changes(false);
    h.settle();
    let _ = h.stoat.render();
    h
}

/// The focused changes view's cursor as a stack row, and its scroll.
fn diff_changes_place(h: &mut stoat::test_harness::TestHarness) -> (usize, usize) {
    let (state, editors) = diff_changes_stack(h);
    diff_changes::layout_stack(state, editors);
    let cursor = diff_changes::cursor_stack_row(state, editors).expect("a cursor");
    (cursor, state.scroll)
}

/// The focused changes view's state, with the editors its excerpts are.
fn diff_changes_stack(
    h: &mut stoat::test_harness::TestHarness,
) -> (&mut DiffChangesState, &mut SlotMap<EditorId, EditorState>) {
    let ws = h.stoat.active_workspace_mut();
    let id = ws
        .focused_diff_changes_id()
        .expect("the changes view is open");
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    (&mut diff_changes_views[id], editors)
}

/// The focused excerpt's file and its cursor's buffer position.
fn diff_changes_cursor(h: &mut stoat::test_harness::TestHarness) -> (String, Point) {
    let (state, editors) = diff_changes_stack(h);
    let excerpt = state.focused_excerpt().expect("a focused excerpt");
    let editor = editors
        .get_mut(excerpt.editor_id)
        .expect("an excerpt editor");
    (excerpt.rel_path.clone(), diff_changes::cursor_point(editor))
}

/// The changes view's rows, and the first column of an excerpt's text.
fn diff_changes_columns(h: &mut stoat::test_harness::TestHarness) -> (Rect, u16) {
    let list = {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).area
    };
    let (state, editors) = diff_changes_stack(h);
    let text = editors[state.excerpts[0].editor_id]
        .text_rect
        .expect("the paint wrote the excerpt's text column");
    (list, text.x)
}

/// A press on an excerpt row puts that file's cursor on the cell under the
/// pointer, and a press left of the text at the start of the row.
#[test]
fn a_press_over_the_changes_view_puts_the_cursor_on_its_cell() {
    let mut h = diff_changes_harness(MinimapMode::Single, 20);
    let (list, text_x) = diff_changes_columns(&mut h);
    let mut press = |x, y| {
        h.stoat
            .update(mouse_event(MouseEventKind::Down(MouseButton::Left), x, y));
        diff_changes_cursor(&mut h)
    };

    // Each one-line file takes a header row, its changed row, and the empty
    // row after its newline, so f01's changed row sits on row 4.
    let pressed = [press(text_x + 1, list.y + 4), press(list.x, list.y + 1)];
    let at = |path: &str, column| (path.to_string(), Point::new(0, column));
    assert_eq!(pressed, [at("f01.txt", 1), at("f00.txt", 0)]);
}

/// A middle press runs goto definition and a right press runs hover, each
/// from the cell under the pointer, as the same buttons do in an editor.
#[test]
fn a_middle_or_right_press_over_the_changes_view_places_the_cursor_and_asks_the_server() {
    let mut h = diff_changes_harness(MinimapMode::Single, 20);
    h.allow_host_swap();
    h.stoat.set_lsp_host(Arc::new(NoopLsp));
    let (list, text_x) = diff_changes_columns(&mut h);
    let mut press = |button, y| {
        h.stoat
            .update(mouse_event(MouseEventKind::Down(button), text_x + 1, y));
        (diff_changes_cursor(&mut h), h.stoat.pending_message.take())
    };

    // Every one-line file takes three stack rows, so f02's changed row is
    // row 7.
    let pressed = [
        press(MouseButton::Middle, list.y + 4),
        press(MouseButton::Right, list.y + 7),
    ];
    let asked = |path: &str| {
        let unavailable = "lsp: no language server running".to_string();
        ((path.to_string(), Point::new(0, 1)), Some(unavailable))
    };
    assert_eq!(
        pressed,
        [asked("f01.txt"), asked("f02.txt")],
        "each press puts the cursor on its cell, then asks the server"
    );
}

/// A press outside an open hover popup closes it and still lands on the
/// stack, as a press outside the popup does over an editor.
#[test]
fn a_press_outside_a_hover_popup_over_the_changes_view_closes_it_and_lands() {
    let mut h = diff_changes_harness(MinimapMode::Single, 20);
    let (list, text_x) = diff_changes_columns(&mut h);

    // The popup's area stays empty until a paint lays it out, so every
    // press lands outside it.
    let editor_id = h.stoat.focused_editor_ids().expect("a focused excerpt").0;
    h.stoat.pending_hover = Some(HoverPopup::new(
        vec![vec![("hi".to_string(), Style::default())]],
        0,
        editor_id,
    ));

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        text_x + 1,
        list.y + 4,
    ));
    assert_eq!(
        (h.stoat.pending_hover.is_none(), diff_changes_cursor(&mut h)),
        (true, ("f01.txt".to_string(), Point::new(0, 1))),
        "the press closes the popup and puts f01's cursor on its cell"
    );
}

/// A wheel notch over the stack scrolls it and leaves the cursor, which
/// stays off screen through the next paint as an editor's wheel strands
/// its cursor.
#[test]
fn a_wheel_over_the_changes_view_scrolls_the_stack_and_strands_the_cursor() {
    let mut h = diff_changes_harness(MinimapMode::Single, 20);
    let (list, _) = diff_changes_columns(&mut h);
    let before = diff_changes_place(&mut h);

    for _ in 0..2 {
        h.stoat.update(mouse_event(
            MouseEventKind::ScrollDown,
            list.x + 2,
            list.y + 1,
        ));
    }
    let _ = h.stoat.render();
    assert_eq!(
        (before, diff_changes_place(&mut h)),
        ((1, 0), (1, 2)),
        "two notches scroll two rows past the cursor's row, and the paint leaves them",
    );
}

/// A press on the pinned header folds its file to the header, as a press on
/// the header in place does. The file's rows leave the stack, the stack
/// scrolls back so the folded header is the view's first row, and the
/// focus moves to the next file's first row, under its header on row 1.
#[test]
fn a_press_on_the_pinned_header_folds_its_file() {
    let mut h = two_changed_files();
    diff_changes_stack(&mut h).0.scroll = 3;
    let _ = h.stoat.render();
    let (list, _) = diff_changes_columns(&mut h);

    left_press(&mut h, list.x + 2, list.y);
    let _ = h.stoat.render();
    assert_eq!(
        (
            first_file_rows(&mut h),
            diff_changes_place(&mut h),
            diff_changes_cursor(&mut h).0,
        ),
        (0, (2, 0), "b.txt".to_string()),
    );
}

/// A second press on a folded header opens its file, whose rows come back
/// under the header. The focus stays on the file it moved to at the fold.
#[test]
fn a_second_press_on_a_folded_header_opens_its_file() {
    let mut h = two_changed_files();
    let (list, _) = diff_changes_columns(&mut h);
    let shown = first_file_rows(&mut h);

    let press = |h: &mut stoat::test_harness::TestHarness| {
        left_press(h, list.x + 2, list.y);
        let _ = h.stoat.render();
        (first_file_rows(h), diff_changes_cursor(h).0)
    };
    let folded = press(&mut h);
    let opened = press(&mut h);
    assert_eq!(
        (folded, opened),
        ((0, "b.txt".to_string()), (shown, "b.txt".to_string())),
        "a.txt shows {shown} rows open",
    );
}

/// Two files of twelve changed lines under a focused changes view at 120 by
/// 14 with no minimap, painted.
fn two_changed_files() -> stoat::test_harness::TestHarness {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 14);
    h.stoat.settings.editor_minimap = Some(MinimapMode::Off);
    let base: String = (1..=12).map(|n| format!("{n}\n")).collect();
    let text: String = (1..=12).map(|n| format!("{n}!\n")).collect();
    h.stage_review_scenario("/repo", &[("a.txt", &base, &text), ("b.txt", &base, &text)]);
    h.open_file(Path::new("/repo/a.txt"));
    h.cover_with_diff_changes(false);
    h.settle();
    let _ = h.stoat.render();
    h
}

/// The display rows the focused changes view's first file shows.
fn first_file_rows(h: &mut stoat::test_harness::TestHarness) -> usize {
    let (state, editors) = diff_changes_stack(h);
    diff_changes::excerpt_rows(&state.excerpts[0], editors)
}

/// A press on the pinned Scope acts as a press on the fold row it repeats.
/// The cursor lands on that fold row, and the stack scrolls back so the
/// header and the fold row show in place.
#[test]
fn a_press_on_the_pinned_scope_lands_on_its_fold_row() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 13);
    h.stoat.settings.editor_minimap = Some(MinimapMode::Off);
    let (base, text) = stoat::test_harness::greeter_rust_file();
    h.stage_review_scenario("/repo", &[("a.rs", &base, &text)]);
    h.open_file(Path::new("/repo/a.rs"));
    h.cover_with_diff_changes(false);
    h.settle();
    // The paint asks for the parse, and the refold after it lands reads
    // the Scopes.
    let _ = h.stoat.render();
    for _ in 0..2 {
        h.stoat.drive_background();
        h.settle();
    }
    let _ = h.stoat.render();
    diff_changes_stack(&mut h).0.scroll = 20;
    let _ = h.stoat.render();
    let (list, _) = diff_changes_columns(&mut h);

    left_press(&mut h, list.x + 2, list.y + 1);
    let _ = h.stoat.render();
    assert_eq!(
        (
            diff_changes_cursor(&mut h).1.row,
            diff_changes_place(&mut h)
        ),
        (0, (1, 0)),
    );
}

/// A press on a per-pane changes view's strip puts the cursor on the row
/// under it, scrolled toward the middle of the view, and a drag follows the
/// pointer until the release. A wheel notch over the strip scrolls the
/// stack and leaves the cursor, as one over the rows does.
#[test]
fn the_changes_view_strip_scrubs_its_changes_view() {
    let mut h = diff_changes_harness(MinimapMode::PerPane, 20);
    let id = h
        .stoat
        .active_workspace()
        .focused_diff_changes_id()
        .expect("the changes view is open");
    let (strip, rows, view) = {
        let (state, editors) = diff_changes_stack(&mut h);
        let rows = diff_changes::layout_stack(state, editors);
        let strip = state.minimap_rect.expect("the paint reserved a strip");
        (strip, rows, state.viewport_rows)
    };
    // Each excerpt takes a header row and two diff rows. The strip's top
    // cell points at its middle line, stack row 3, which is the second
    // excerpt's header, so the cursor lands on that excerpt's first row.
    let top_cell_row = 4;

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        strip.x,
        strip.bottom() - 1,
    ));
    let pressed = (diff_changes_place(&mut h), h.stoat.minimap_drag);
    h.stoat.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        strip.x,
        strip.y,
    ));
    let dragged = diff_changes_place(&mut h);
    h.stoat.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        strip.x,
        strip.y,
    ));
    let released = h.stoat.minimap_drag;
    h.stoat
        .update(mouse_event(MouseEventKind::ScrollDown, strip.x, strip.y));
    assert_eq!(
        (pressed, dragged, released, diff_changes_place(&mut h)),
        (
            ((rows - 1, rows - view), Some(MinimapDrag::DiffChanges(id))),
            (top_cell_row, 0),
            None,
            (top_cell_row, 1),
        ),
    );
}

/// A stack longer than the strip slides the strip's window with the
/// scroll, as the strip the terminal draws does, so a drag held on the
/// strip's last cell walks further down the stack. A press that lands on a
/// header row puts the cursor on its excerpt's first row.
#[test]
fn a_long_changes_view_scrubs_against_its_sliding_strip() {
    let mut h = diff_changes_harness(MinimapMode::PerPane, 100);
    let strip = h
        .stoat
        .active_workspace()
        .focused_diff_changes()
        .and_then(|state| state.minimap_rect)
        .expect("the paint reserved a strip");
    let last_cell = |kind| mouse_event(kind, strip.x, strip.bottom() - 1);

    h.stoat
        .update(last_cell(MouseEventKind::Down(MouseButton::Left)));
    let pressed = diff_changes_place(&mut h);
    h.stoat
        .update(last_cell(MouseEventKind::Drag(MouseButton::Left)));
    assert_eq!(
        (pressed, diff_changes_place(&mut h)),
        ((136, 124), (208, 196))
    );
}

/// A press on the strip of a changes view with no excerpt arms the scrub
/// and moves nothing.
#[test]
fn a_strip_press_without_an_excerpt_moves_nothing() {
    let mut h = diff_changes_harness(MinimapMode::PerPane, 20);
    let id = h
        .stoat
        .active_workspace()
        .focused_diff_changes_id()
        .expect("the changes view is open");
    let strip = {
        let (state, _) = diff_changes_stack(&mut h);
        state.excerpts.clear();
        state.minimap_rect.expect("the paint reserved a strip")
    };

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        strip.x,
        strip.bottom() - 1,
    ));
    let drag = h.stoat.minimap_drag;
    let (state, _) = diff_changes_stack(&mut h);
    assert_eq!(
        (state.scroll, state.excerpts.len(), drag),
        (0, 0, Some(MinimapDrag::DiffChanges(id))),
    );
}

/// The single band scrubs a focused changes view, as it scrubs a focused
/// editor.
///
/// The band's top cell points at stack row 3, the second excerpt's header,
/// so the cursor lands on that excerpt's first row.
#[test]
fn the_single_band_scrubs_a_focused_changes_view() {
    let mut h = diff_changes_harness(MinimapMode::Single, 20);
    let band = h
        .stoat
        .single_minimap_rect
        .expect("single mode reserves a band");
    let id = h
        .stoat
        .active_workspace()
        .focused_diff_changes_id()
        .expect("the changes view is open");

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        band.x,
        band.y,
    ));
    assert_eq!(
        (diff_changes_place(&mut h), h.stoat.minimap_drag),
        ((4, 0), Some(MinimapDrag::DiffChanges(id))),
    );
}

/// Seed a repo with `n` commits, open the commits screen over a long
/// buffer, and hand back the harness plus the list rect the paint uses.
fn commits_harness(n: usize) -> (stoat::test_harness::TestHarness, Rect) {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 40);
    h.seed_focused_buffer(&"line\n".repeat(200));
    let commits: Vec<_> = (0..n)
        .map(|i| {
            (
                format!("c{i}"),
                format!("commit {i}"),
                vec![("a.rs", "fn a() {}\n")],
            )
        })
        .collect();
    let specs: Vec<_> = commits
        .iter()
        .map(|(sha, msg, files)| (sha.as_str(), msg.as_str(), files.as_slice()))
        .collect();
    h.seed_linear_history("/repo", &specs);
    h.open_commits("/repo");

    let pane = {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).area
    };
    let list = stoat::render::commits::commits_list_rect(pane, h.stoat.commits_split)
        .expect("the list fits this terminal");
    (h, list)
}

/// The hidden editor's scroll row, which every commits pointer test pins
/// so a gesture the screen claims is never seen underneath it.
fn hidden_scroll_row(h: &stoat::test_harness::TestHarness) -> u32 {
    let ws = h.stoat.active_workspace();
    let covered = ws.focused_commits().and_then(|list| list.covered.as_ref());
    let Some(View::Editor(id)) = covered else {
        panic!("the commits list covers no editor");
    };
    ws.editors[*id].scroll_row
}

fn selected_commit(h: &stoat::test_harness::TestHarness) -> usize {
    h.stoat
        .active_workspace()
        .focused_commits()
        .expect("the commits screen is open")
        .selected
}

fn detail_scroll(h: &stoat::test_harness::TestHarness) -> usize {
    h.stoat
        .active_workspace()
        .focused_commits()
        .expect("the commits screen is open")
        .preview_scroll
}

/// A press resolves to the commit drawn under the pointer, under the graph
/// header's row, and a press on the header selects nothing.
#[test]
fn a_press_lands_on_the_commit_drawn_under_it() {
    let (mut h, list) = commits_harness(4);
    action_handlers::dispatch(&mut h.stoat, &stoat_action::CommitsLast);
    h.snapshot();
    let row_of = |h: &stoat::test_harness::TestHarness, text: &str| {
        let rows = h.rendered_text();
        rows.lines()
            .position(|line| line.contains(text))
            .expect("drawn") as u16
    };
    let (header, drawn) = (row_of(&h, "graph"), row_of(&h, "commit 1"));
    let press = |h: &mut stoat::test_harness::TestHarness, row: u16| {
        let down = MouseEventKind::Down(MouseButton::Left);
        h.stoat.update(mouse_event(down, list.x + 2, row));
        selected_commit(h)
    };

    assert_eq!(
        [press(&mut h, header), press(&mut h, drawn)],
        [3, 2],
        "the header selects nothing, and a row selects the commit drawn there"
    );
}

#[test]
fn a_wheel_over_the_commits_list_steps_the_selection() {
    let (mut h, list) = commits_harness(8);
    let before = hidden_scroll_row(&h);

    h.stoat.update(mouse_event(
        MouseEventKind::ScrollDown,
        list.x + 2,
        list.y + 1,
    ));

    assert_eq!(
        (selected_commit(&h), hidden_scroll_row(&h)),
        (1, before),
        "the notch walks the list and never reaches the editor beneath"
    );
}

/// The user asked for the wheel to scroll whichever pane it sits over, so
/// the detail side moves its own diff and leaves the selection where it
/// is. The hidden editor never sees the notch either way.
#[test]
fn a_wheel_over_the_commits_detail_side_scrolls_the_diff() {
    let (mut h, list) = commits_harness(8);
    let before = hidden_scroll_row(&h);

    h.stoat.update(mouse_event(
        MouseEventKind::ScrollDown,
        list.x + list.width + 4,
        list.y + 1,
    ));

    assert_eq!(
        (
            selected_commit(&h),
            detail_scroll(&h),
            hidden_scroll_row(&h),
        ),
        (0, PREVIEW_WHEEL_ROWS, before),
        "the diff scrolls and neither the list nor the editor beneath moves"
    );
}

/// The offset belongs to the diff under the selection, so stepping to a
/// different commit starts its diff at the top.
#[test]
fn stepping_the_commit_selection_resets_the_detail_scroll() {
    let (mut h, list) = commits_harness(8);
    h.stoat.update(mouse_event(
        MouseEventKind::ScrollDown,
        list.x + list.width + 4,
        list.y + 1,
    ));
    assert_eq!(
        detail_scroll(&h),
        PREVIEW_WHEEL_ROWS,
        "the diff is scrolled before the step"
    );

    h.stoat.update(mouse_event(
        MouseEventKind::ScrollDown,
        list.x + 2,
        list.y + 1,
    ));

    assert_eq!(
        (selected_commit(&h), detail_scroll(&h)),
        (1, 0),
        "the new commit's diff starts at its top"
    );
}

/// A press selects the clicked commit and nothing more. The user named
/// "not activate, just select", so a second press on the same row leaves
/// the screen up and checks out nothing.
#[test]
fn a_click_on_a_commit_row_selects_it_without_activating() {
    let (mut h, list) = commits_harness(8);
    let workdir = PathBuf::from("/repo");

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        list.x + 2,
        list.y + 2,
    ));
    assert_eq!(
        (selected_commit(&h), h.fake_git().checkouts(&workdir)),
        (2, Vec::new()),
        "the third row is selected and nothing is checked out"
    );

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        list.x + 2,
        list.y + 2,
    ));
    assert_eq!(
        (
            selected_commit(&h),
            stoat::keymap_state::view_predicate(h.stoat.active_workspace()),
            h.fake_git().checkouts(&workdir),
        ),
        (2, Some("commits"), Vec::new()),
        "a second press on the selected row acts on nothing"
    );
}

/// The list rect runs the height of the pane, so most of it is empty on a
/// short history. A press there resolves to no row and is swallowed all
/// the same.
#[test]
fn a_click_below_the_last_commit_changes_nothing() {
    let (mut h, list) = commits_harness(3);
    let before = (selected_commit(&h), hidden_cursors(&mut h));

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        list.x + 2,
        list.y + list.height - 1,
    ));

    assert_eq!(
        (selected_commit(&h), hidden_cursors(&mut h)),
        before,
        "an empty row moves neither the list nor the cursor beneath"
    );
}

/// The cursors of the editor the commits list covers.
fn hidden_cursors(h: &mut stoat::test_harness::TestHarness) -> Vec<(u32, u32)> {
    let ws = h.stoat.active_workspace_mut();
    let covered = ws.focused_commits().and_then(|list| list.covered.clone());
    let Some(View::Editor(id)) = covered else {
        panic!("the commits list covers no editor");
    };
    stoat::test_harness::editor::editor_cursor_buffer_positions(&mut ws.editors[id])
}

/// Seed a repo, open the rebase screen over a long buffer, and hand back
/// the harness plus the todo list rect the paint uses.
fn rebase_harness() -> (stoat::test_harness::TestHarness, Rect) {
    let (mut h, _) = commits_harness(8);
    h.type_keys("G");
    h.type_keys("i");
    assert_eq!(
        h.stoat.current_view(),
        Some("rebase"),
        "the rebase screen opened"
    );

    let pane = {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).area
    };
    let list =
        stoat::render::rebase::rebase_list_rect(pane).expect("the todo list fits this terminal");
    (h, list)
}

fn rebase_selected(h: &stoat::test_harness::TestHarness) -> usize {
    h.stoat
        .active_workspace()
        .rebase
        .as_ref()
        .expect("the rebase screen is open")
        .selected
}

#[test]
fn a_wheel_over_the_rebase_screen_steps_its_selection() {
    let (mut h, list) = rebase_harness();
    let before = hidden_scroll_row(&h);

    h.stoat.update(mouse_event(
        MouseEventKind::ScrollDown,
        list.x + 2,
        list.y + 1,
    ));

    assert_eq!(
        (rebase_selected(&h), hidden_scroll_row(&h)),
        (1, before),
        "the notch steps the plan and never reaches the editor beneath"
    );
}

/// A press picks the entry to act on and nothing else. Running the plan
/// stays on its own key, so a misclick rewrites no history.
#[test]
fn a_click_on_a_rebase_row_selects_it_without_changing_its_op() {
    let (mut h, list) = rebase_harness();
    let ops = |h: &stoat::test_harness::TestHarness| {
        h.stoat
            .active_workspace()
            .rebase
            .as_ref()
            .expect("open")
            .todo
            .iter()
            .map(|entry| entry.op)
            .collect::<Vec<_>>()
    };
    let before = ops(&h);

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        list.x + 2,
        list.y + 1,
    ));

    assert_eq!(
        (rebase_selected(&h), ops(&h)),
        (1, before),
        "the second row is selected and every entry keeps its op"
    );
}

/// Seed a rebase that stops on a conflict over two files, and hand back
/// the harness plus the file list rect the paint uses.
fn conflict_harness() -> (stoat::test_harness::TestHarness, Rect) {
    conflict_harness_over(
        &[("a.rs", "a0\n"), ("b.rs", "b0\n")],
        &[("a.rs", "a1\n"), ("b.rs", "b1\n")],
    )
}

/// [`conflict_harness`] over the files of `root` and `tip`.
///
/// The fake conflicts on every path the two trees disagree about, so a file
/// both carry with different text makes one row of the list.
fn conflict_harness_over(
    root: &[(&str, &str)],
    tip: &[(&str, &str)],
) -> (stoat::test_harness::TestHarness, Rect) {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 40);
    h.seed_focused_buffer(&"line\n".repeat(200));
    h.seed_linear_history("/repo", &[("c0", "c0: root", root), ("c1", "c1: tip", tip)]);
    h.fake_git().add_repo("/repo").simulate_conflict_at("c1");
    h.open_commits("/repo");
    h.type_keys("G");
    h.type_keys("i");
    h.type_keys("Enter");
    assert_eq!(
        h.stoat.current_view(),
        Some("rebase_conflict"),
        "the rebase stopped on a conflict"
    );

    let pane = {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).area
    };
    let list = stoat::render::conflict::conflict_list_rect(pane)
        .expect("the file list fits this terminal");
    (h, list)
}

/// The file count, the selection, and the resolution count, all of which
/// live on the paused rebase. Reported together so a test pins the
/// fixture it depends on alongside what it asserts.
fn conflict_state(h: &stoat::test_harness::TestHarness) -> (usize, usize, usize) {
    let RebasePause::Conflict {
        files,
        selected,
        resolutions,
        ..
    } = h
        .stoat
        .active_workspace()
        .rebase_active
        .as_ref()
        .expect("a rebase is paused")
        .pause
        .as_ref()
        .expect("on a conflict")
    else {
        panic!("the pause is not a conflict")
    };
    (files.len(), *selected, resolutions.len())
}

#[test]
fn a_count_steps_the_conflict_file_list() {
    let (mut h, _) = conflict_harness_over(
        &[("a.rs", "a0\n"), ("b.rs", "b0\n"), ("c.rs", "c0\n")],
        &[("a.rs", "a1\n"), ("b.rs", "b1\n"), ("c.rs", "c1\n")],
    );

    h.type_keys("2 j");
    let down = conflict_state(&h);
    h.type_keys("9 k");

    assert_eq!(
        (down, conflict_state(&h)),
        ((3, 2, 0), (3, 0, 0)),
        "a count steps that many files and stops at either end"
    );
}

#[test]
fn a_wheel_over_the_conflict_screen_steps_the_file_selection() {
    let (mut h, list) = conflict_harness();
    let before = hidden_scroll_row(&h);

    h.stoat
        .update(mouse_event(MouseEventKind::ScrollDown, list.x + 2, list.y));

    assert_eq!(
        (conflict_state(&h), hidden_scroll_row(&h)),
        ((2, 1, 0), before),
        "the notch steps the file list, resolves nothing, and never \
             reaches the editor beneath"
    );
}

/// A press picks which file to look at. Taking a side stays on its own
/// key, so a misclick resolves nothing.
#[test]
fn a_click_on_a_conflict_row_selects_it_without_resolving_it() {
    let (mut h, list) = conflict_harness();

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        list.x + 2,
        list.y + 1,
    ));

    assert_eq!(
        conflict_state(&h),
        (2, 1, 0),
        "the second file is selected and neither is resolved"
    );
}

/// An open boxed modal owns the wheel wherever the pointer sits, so the
/// pane it covers never scrolls under it.
#[test]
fn a_wheel_over_code_search_walks_its_list_and_not_the_pane() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 40);
    h.seed_focused_buffer(&"line\n".repeat(200));
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCodeSearch);
    h.stoat
        .code_search
        .as_mut()
        .expect("code search open")
        .matches = (0..20)
        .map(|i| stoat::code_search::SearchMatch {
            path: Arc::from(Path::new("/a.rs")),
            line: i,
            column: 0,
            offset: 0,
            snippet: format!("hit {i}"),
        })
        .collect();

    let scroll_row = |h: &mut stoat::test_harness::TestHarness| {
        let ws = h.stoat.active_workspace();
        let id = match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        ws.editors[id].scroll_row
    };
    let before = scroll_row(&mut h);
    h.stoat
        .update(mouse_event(MouseEventKind::ScrollDown, 20, 20));

    assert_eq!(
        (
            h.stoat.code_search.as_ref().expect("still open").selected,
            scroll_row(&mut h),
        ),
        (1, before),
        "the wheel steps the match list and leaves the covered pane alone"
    );
}

/// A press inside an open modal belongs to the modal, so the buffer under
/// it keeps the cursor it had.
#[test]
fn a_click_inside_the_jumplist_modal_leaves_the_editor_cursor() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 40);
    h.seed_focused_buffer("alpha\nbeta\ngamma\n");
    action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    action_handlers::dispatch(&mut h.stoat, &MoveDown);
    action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenJumplistPicker);
    h.snapshot();

    let before = stoat::test_harness::editor::head_offsets(&mut h.stoat);
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 60, 20));

    assert_eq!(
        stoat::test_harness::editor::head_offsets(&mut h.stoat),
        before,
        "the press never reaches the buffer the modal covers"
    );
}

/// The bottom-line prompts leave the editor visible, so a press there is
/// still the editor's. This pins that exclusion.
#[test]
fn a_click_with_the_search_prompt_open_still_places_the_cursor() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 40);
    h.seed_focused_buffer("alpha\nbeta\ngamma\n");
    h.type_keys("/");
    h.snapshot();
    assert!(h.stoat.search_input.is_some(), "the prompt is open");

    let before = stoat::test_harness::editor::head_offsets(&mut h.stoat);
    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 3, 2));

    assert_ne!(
        stoat::test_harness::editor::head_offsets(&mut h.stoat),
        before,
        "a bottom-line prompt does not take the pointer"
    );
}

/// The column where the `nth` `label` starts on the composited tab bar row,
/// counting from zero.
fn tab_label_col(h: &mut stoat::test_harness::TestHarness, label: &str, nth: usize) -> u16 {
    let buf = h.render_composited();
    let row: String = (0..buf.area.width).map(|x| buf[(x, 0)].symbol()).collect();
    let (byte, _) = row
        .match_indices(label)
        .nth(nth)
        .expect("the label is painted");
    // A cell holds one char here, so a char count is the column.
    row[..byte].chars().count() as u16
}

#[test]
fn a_left_press_on_a_tab_switches_to_it() {
    let mut h = Stoat::test();
    action_handlers::dispatch(&mut h.stoat, &NewTab);
    let active_after_press = |h: &mut stoat::test_harness::TestHarness, nth: usize| {
        let column = tab_label_col(h, "scratch", nth);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            column,
            0,
        ));
        h.stoat.active_workspace().active_tab
    };

    assert_eq!(
        [active_after_press(&mut h, 0), active_after_press(&mut h, 1)],
        [0, 1],
    );
}

/// A tab bar band takes the top two rows, so a left press at the first
/// tab's middle column switches to it from either row. Without the band,
/// the second row is the first pane row, and a press there keeps the tab.
///
/// The press reads the tab's extent from the bar's painted spans, in
/// sixteenths of a cell from the bar's left edge.
#[test]
fn a_press_on_either_row_of_a_tab_bar_band_switches_tabs() {
    let active_after_press = |protocol: u32, row: u16| {
        let mut h = Stoat::test();
        h.stoat.stoatty_protocol = protocol;
        action_handlers::dispatch(&mut h.stoat, &NewTab);
        h.stoat.render();
        let first = h.stoat.tab_bar_spans[0].clone();
        let column = (first.start + first.end) / 32;
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            column,
            row,
        ));
        h.stoat.active_workspace().active_tab
    };

    assert_eq!(
        [
            active_after_press(NUDGE_PROTOCOL, 0),
            active_after_press(NUDGE_PROTOCOL, 1),
            active_after_press(NUDGE_PROTOCOL - 1, 1),
        ],
        [0, 0, 1],
        "the active tab after a press on row 0 and row 1 of a band, then on row 1 under a \
             one-row bar",
    );
}

/// The bar row sits above every pane, so a press there that switches no tab
/// still stays off the focused pane's first row. The workspace name leads
/// the bar and names no tab.
#[test]
fn a_press_that_switches_no_tab_stops_at_the_bar() {
    let mut h = Stoat::test();
    action_handlers::dispatch(&mut h.stoat, &NewTab);
    h.seed_focused_buffer("alpha\nbeta\ngamma\n");
    action_handlers::dispatch(&mut h.stoat, &MoveDown);
    let state = |h: &mut stoat::test_harness::TestHarness| {
        (
            h.stoat.active_workspace().active_tab,
            stoat::test_harness::editor::head_offsets(&mut h.stoat),
        )
    };
    let before = state(&mut h);
    let first_tab = tab_label_col(&mut h, "scratch", 0);
    let name = tab_label_col(&mut h, "(unnamed)", 0);
    let edge = h.stoat.size().width - 1;

    let after = [
        (MouseButton::Left, edge),
        (MouseButton::Left, name),
        (MouseButton::Right, first_tab),
        (MouseButton::Middle, first_tab),
    ]
    .map(|(button, column)| {
        h.stoat
            .update(mouse_event(MouseEventKind::Down(button), column, 0));
        state(&mut h)
    });

    assert_eq!(
        after,
        [before.clone(), before.clone(), before.clone(), before]
    );
}

/// The checkout banner's row sits above every pane too, so a press there
/// leaves the focus and the cursor where they were. With the tab bar shown,
/// the banner takes row 1.
#[test]
fn a_press_on_the_checkout_banner_stops_at_the_banner() {
    let mut h = Stoat::test();
    action_handlers::dispatch(&mut h.stoat, &NewTab);
    h.seed_focused_buffer("alpha\nbeta\ngamma\n");
    action_handlers::dispatch(&mut h.stoat, &MoveDown);
    h.stoat.active_workspace_mut().review_walk = Some(ReviewWalk::blame_checkout(
        "c1",
        ReturnRef::Branch("main".into()),
        None,
    ));
    h.stoat.render();
    let state = |h: &mut stoat::test_harness::TestHarness| {
        (
            h.stoat.active_workspace().panes.focus(),
            stoat::test_harness::editor::head_offsets(&mut h.stoat),
        )
    };
    let before = state(&mut h);

    h.stoat
        .update(mouse_event(MouseEventKind::Down(MouseButton::Left), 3, 1));
    assert_eq!(state(&mut h), before);
}

/// A `width` by `height` harness rooted in `/repo` with `files` unstaged
/// files, its git status panel open and painted.
fn git_status_harness(width: u16, height: u16, files: usize) -> stoat::test_harness::TestHarness {
    let mut h = stoat::test_harness::TestHarness::with_size(width, height);
    {
        let mut repo = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
        repo.set_head_branch("main");
        for i in 0..files {
            repo.modified(format!("f{i:02}.rs"), "a\n", "b\n");
        }
    }
    h.stoat.active_workspace_mut().git_root = "/repo".into();
    action_handlers::dispatch(&mut h.stoat, &stoat_action::GitStatus);
    h.settle();
    h.snapshot();
    h
}

fn left_press(h: &mut stoat::test_harness::TestHarness, column: u16, row: u16) {
    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));
}

/// A left press on a Tray Chip opens the Glance of its Job, and while the
/// Glance shows, a press on another chip moves the Glance to that Job.
#[test]
fn a_press_on_a_tray_chip_opens_its_glance() {
    let mut h = stoat::test_harness::TestHarness::default();
    let older = h.push_job("sleep 30");
    let newer = h.push_job("sleep 31");
    h.snapshot();
    let chips: Vec<u16> = h
        .stoat
        .tray_spans
        .iter()
        .map(|span| span.cells.start)
        .collect();

    let mut named = Vec::new();
    for column in chips {
        left_press(&mut h, column, 0);
        named.push(h.stoat.job_glance.as_ref().map(|glance| glance.cmd));
    }
    assert_eq!(named, [Some(older.cmd), Some(newer.cmd)]);
}

/// A harness whose tray holds the chips of `sleep 30` and `sleep 31` at
/// the board's pixels, 425 to 566 and 570 to 711, with the Cmds of their
/// Jobs. The tray's region starts at pixel 423, the left edge of the cell
/// 47.
fn pixel_chips() -> (stoat::test_harness::TestHarness, CmdId, CmdId) {
    let mut h = stoat::test_harness::TestHarness::default();
    h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
    h.stoat.cell_pixels = Some((9, 18));
    h.stoat.display_scale = Some(1.0);
    let older = h.push_job("sleep 30").cmd;
    let newer = h.push_job("sleep 31").cmd;
    h.snapshot();
    (h, older, newer)
}

/// A press on a chip at the board's pixels reads the pixel that the tray's
/// pool reports. The pixels 143, 147, and 145 of its region are the last
/// pixel of the older chip, the first pixel of the newer chip, and one of
/// the 3 px between the two, which names no chip and closes the Glance.
#[test]
fn a_press_on_a_chip_reads_the_pointer_s_pixel() {
    let (mut h, older, newer) = pixel_chips();
    let mut named = Vec::new();
    for (col, x) in [(62, 143), (63, 147), (63, 145)] {
        let press = WindowIpcEvent::PoolMouse {
            pool: stoat::smooth_scroll::non_pane_pool::JOB_TRAY,
            kind: MouseKind::Press(IpcMouseButton::Left),
            col,
            row: 0,
            cell_col: x,
            cell_row: 9,
            mods: 0,
        };
        stoat::app::deliver_window_event(&mut h.stoat, press);
        named.push(h.stoat.job_glance.as_ref().map(|glance| glance.cmd));
    }

    assert_eq!(named, [Some(older), Some(newer), None]);
}

/// With no pixel report, a press on a chip at the board's pixels reads the
/// middle pixel of its cell. The cell 63 holds the 3 px between the chips
/// and the first 6 px of the newer chip, and its middle pixel 571 is the
/// newer chip's.
#[test]
fn a_chip_press_with_no_pixel_report_takes_the_middle_of_its_cell() {
    let (mut h, _, newer) = pixel_chips();
    left_press(&mut h, 63, 0);

    assert_eq!(
        h.stoat.job_glance.as_ref().map(|glance| glance.cmd),
        Some(newer),
    );
}

/// A press on a row of the Job List selects it, and a second press on the
/// selected row opens it, which closes the list.
#[test]
fn a_press_on_a_job_list_row_selects_it_and_a_second_opens_it() {
    let mut h = stoat::test_harness::TestHarness::default();
    h.open_run();
    let echo = h.submit_run("echo a");
    h.inject_cmd_done(echo.cmd, 0);
    h.advance_clock(Duration::from_secs(1));
    h.push_job("sleep 30");
    h.type_keys("escape space J j");
    h.snapshot();
    let inner = stoat::render::job_list::job_list_layout(h.stoat.size(), 2, None)
        .expect("the list fits")
        .inner;
    let row = stoat::render::job_list::job_list_rows(inner).y + 1;

    left_press(&mut h, inner.x + 5, row);
    let selected = h.stoat.job_list.as_ref().map(|list| list.selected());
    left_press(&mut h, inner.x + 5, row);
    assert_eq!((selected, h.stoat.job_list.is_none()), (Some(1), true));
}

/// A press on a pixel Job List reads the pointer's pixel down the rows
/// under its column header. The first row's box stands 5 px down and each
/// row 21 px under the one before it, so the pixel 26 is the second row's,
/// the pixel 3 over the first row's box is no row's and keeps the
/// selection, and the pixel 25, the gap under the first row, is the first
/// row's.
#[test]
fn a_press_on_a_pixel_job_row_selects_the_row_under_the_pointer() {
    let mut h = stoat::test_harness::TestHarness::default();
    h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
    h.stoat.cell_pixels = Some((9, 18));
    h.stoat.display_scale = Some(1.0);
    h.push_job("sleep 30");
    h.push_job("sleep 31");
    h.type_keys("escape space J j");
    h.snapshot();
    let metrics = h.stoat.job_list.as_ref().and_then(|list| list.row_metrics);
    let rows = stoat::render::job_list::job_list_layout(h.stoat.size(), 2, metrics)
        .expect("the list fits")
        .rows;

    let mut selected = Vec::new();
    for pixel in [26, 3, 25] {
        let press = WindowIpcEvent::PoolMouse {
            pool: stoat::smooth_scroll::non_pane_pool::JOB_LIST,
            kind: MouseKind::Press(IpcMouseButton::Left),
            col: rows.x + 5,
            row: rows.y + pixel / 18,
            cell_col: 45,
            cell_row: pixel,
            mods: 0,
        };
        stoat::app::deliver_window_event(&mut h.stoat, press);
        selected.push(h.stoat.job_list.as_ref().map(|list| list.selected()));
    }

    assert_eq!(
        (metrics.is_some(), selected),
        (true, vec![Some(1), Some(1), Some(0)]),
    );
}

/// A pixel Job List of six Jobs on 24 rows shows 4 rows, so a press on
/// the pixel 95 of its rows, where a fifth row's box would stand, names no
/// row and keeps the selection.
#[test]
fn a_press_under_the_last_pixel_job_row_on_show_names_none() {
    let mut h = stoat::test_harness::TestHarness::default();
    h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
    h.stoat.cell_pixels = Some((9, 18));
    h.stoat.display_scale = Some(1.0);
    for n in 0..6 {
        h.push_job(&format!("sleep {n}"));
    }
    h.type_keys("escape space J j");
    h.snapshot();
    let metrics = h.stoat.job_list.as_ref().and_then(|list| list.row_metrics);
    let rows = stoat::render::job_list::job_list_layout(h.stoat.size(), 6, metrics)
        .expect("the list fits")
        .rows;

    let press = WindowIpcEvent::PoolMouse {
        pool: stoat::smooth_scroll::non_pane_pool::JOB_LIST,
        kind: MouseKind::Press(IpcMouseButton::Left),
        col: rows.x + 5,
        row: rows.y + 5,
        cell_col: 45,
        cell_row: 95,
        mods: 0,
    };
    stoat::app::deliver_window_event(&mut h.stoat, press);

    let list = h.stoat.job_list.as_ref();
    assert_eq!(
        (
            list.and_then(|list| list.viewport_rows),
            list.map(|list| list.selected())
        ),
        (Some(4), Some(0)),
    );
}

/// While the Glance shows, a left press off the chips closes it.
#[test]
fn a_press_off_the_chips_closes_the_glance() {
    let mut h = stoat::test_harness::TestHarness::default();
    h.push_job("sleep 30");
    h.type_keys("escape space J k");
    h.snapshot();
    let open = h.stoat.job_glance.is_some();
    left_press(&mut h, 5, 10);

    assert_eq!((open, h.stoat.job_glance.is_none()), (true, true));
}

fn git_status_selected(h: &stoat::test_harness::TestHarness) -> Option<usize> {
    h.stoat.git_status.as_ref().map(|panel| panel.selected)
}

/// A press beside the box or on a section head selects nothing, a press on
/// a file row selects it, and a second press on that row opens its diff.
///
/// At 80 by 24 the box's inner rect starts at column 29 and row 3. Row 3
/// holds the header, row 4 heads the unstaged section, and rows 5 and 6
/// hold its two files.
#[test]
fn presses_on_a_git_status_row_select_it_then_open_it() {
    let mut h = git_status_harness(80, 24, 2);
    left_press(&mut h, 5, 6);
    let beside = git_status_selected(&h);
    left_press(&mut h, 33, 4);
    let on_head = git_status_selected(&h);
    left_press(&mut h, 33, 6);
    let on_row = git_status_selected(&h);
    left_press(&mut h, 33, 6);
    h.settle();

    assert_eq!(
        (
            beside,
            on_head,
            on_row,
            active_modal(&h.stoat),
            h.stoat.current_view()
        ),
        (Some(0), Some(0), Some(1), None, Some("diff")),
    );
}

/// A press in a scrolled body names the row drawn under it.
///
/// At 80 by 14 the body holds eight lines. Selecting the last of twelve
/// files scrolls the body to start at the sixth, which row 5 shows under
/// the head on row 4.
#[test]
fn a_press_in_a_scrolled_git_status_body_names_the_row_drawn_there() {
    let mut h = git_status_harness(80, 14, 12);
    for _ in 0..11 {
        h.type_keys("j");
    }
    h.snapshot();
    left_press(&mut h, 33, 5);

    assert_eq!(git_status_selected(&h), Some(5));
}

/// Under the checkout banner the box hangs a row lower, so its body holds a
/// line less, and a press names the row drawn under it there.
///
/// At 80 by 14 the banner takes row 0, and the body holds seven lines.
/// Selecting the last of twelve files scrolls the body to start at the
/// seventh, which row 6 shows under the head on row 5.
#[test]
fn a_press_under_the_checkout_banner_names_the_git_status_row_drawn_there() {
    let mut h = git_status_harness(80, 14, 12);
    h.stoat.active_workspace_mut().review_walk = Some(ReviewWalk::blame_checkout(
        "c1",
        ReturnRef::Branch("main".into()),
        None,
    ));
    for _ in 0..11 {
        h.type_keys("j");
    }
    h.snapshot();
    left_press(&mut h, 33, 6);

    assert_eq!(git_status_selected(&h), Some(6));
}

/// The wheel walks the selection one row per line of travel and stops at
/// the first and the last row, as it walks a picker's.
#[test]
fn the_wheel_walks_the_git_status_selection_by_its_travel() {
    let mut h = git_status_harness(80, 24, 4);
    let walk = [2.0, 3.0, -1.0, -5.0].map(|lines: f32| {
        let kind = match lines > 0.0 {
            true => MouseEventKind::ScrollDown,
            false => MouseEventKind::ScrollUp,
        };
        let over_first_row = MouseEvent {
            kind,
            column: 33,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse_scroll(&mut h.stoat, over_first_row, lines);
        git_status_selected(&h)
    });

    assert_eq!(walk, [Some(2), Some(3), Some(2), Some(0)]);
}

/// A 120 by 40 harness with the workspace picker open over one workspace
/// per name, and the picker's inner rect.
fn workspace_picker_harness(names: &[&str]) -> (stoat::test_harness::TestHarness, Rect) {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 40);
    for _ in names {
        h.type_action("NewWorkspace()");
    }
    let active = h.stoat.active_workspace;
    let listed: Vec<_> = h
        .stoat
        .workspaces
        .keys()
        .filter(|&id| id != active)
        .collect();
    for (id, name) in listed.into_iter().zip(names) {
        h.stoat.workspaces[id].name = (*name).into();
    }
    h.type_action("SwitchWorkspace()");

    let (_, inner) =
        stoat::render::workspace_picker::workspace_picker_layout(h.stoat.size(), names.len())
            .expect("the box fits 120 by 40");
    (h, inner)
}

fn workspace_selected(h: &stoat::test_harness::TestHarness) -> Option<usize> {
    h.stoat
        .workspace_picker
        .as_ref()
        .map(|picker| picker.selected())
}

/// A press on the last or the first painted workspace row selects it, and
/// a press on the filter input selects nothing.
///
/// Row 0 of the inner rect holds the filter input, and rows 1 and 2 its
/// rule and the column header, so the four rows paint from row 3.
#[test]
fn presses_on_workspace_picker_rows_select_the_row_drawn_there() {
    let (mut h, inner) = workspace_picker_harness(&["a", "b", "c", "d"]);
    let walk = [inner.y + 6, inner.y + 3, inner.y].map(|row| {
        left_press(&mut h, inner.x + 10, row);
        workspace_selected(&h)
    });

    assert_eq!(walk, [Some(3), Some(0), Some(0)]);
}

/// A press in a scrolled workspace list names the row drawn under it.
///
/// The 40-row harness shows fourteen rows, half its height less the box's
/// six other rows. Sixteen rows then scroll, so with the last one selected
/// the window starts at row 2, which paints at row 3 of the inner rect.
#[test]
fn a_press_in_a_scrolled_workspace_picker_names_the_row_drawn_there() {
    let names = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p",
    ];
    let (mut h, inner) = workspace_picker_harness(&names);
    for _ in 1..names.len() {
        h.type_keys("Down");
    }
    left_press(&mut h, inner.x + 10, inner.y + 3);

    assert_eq!(workspace_selected(&h), Some(2));
}

/// A press on a row past the workspaces the query keeps selects nothing,
/// since the list paints only those.
#[test]
fn a_press_past_the_filtered_workspace_rows_selects_nothing() {
    let (mut h, inner) = workspace_picker_harness(&["alpha", "alps", "beta", "gamma"]);
    h.type_text("alp");
    left_press(&mut h, inner.x + 10, inner.y + 5);

    assert_eq!(workspace_selected(&h), Some(0));
}

/// A press selects the clicked row, and a press on the row already
/// selected acts on it. The two-step keeps a misclick from navigating.
#[test]
fn two_presses_on_a_diagnostics_row_select_it_then_jump() {
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

    let mut h = stoat::test_harness::TestHarness::with_size(120, 40);
    let path = h.write_file("diag.rs", "alpha\nbeta\ngamma\n");
    h.open_file(&path);
    let diag = |line: u32| Diagnostic {
        range: Range::new(Position::new(line, 0), Position::new(line, 1)),
        severity: Some(DiagnosticSeverity::ERROR),
        message: format!("problem {line}"),
        ..Default::default()
    };
    h.seed_diagnostics(path.clone(), vec![diag(0), diag(1), diag(2)]);
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenDiagnosticsPicker);
    h.snapshot();

    let surfaces = boxed_modal_surfaces(&h.stoat).expect("the picker is a boxed modal");
    let (col, second_row) = (surfaces.list.x + 1, surfaces.list.y + 2);

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        col,
        second_row,
    ));
    let selected = h
        .stoat
        .diagnostics_picker
        .as_ref()
        .expect("still open after the first press")
        .selected();

    h.stoat.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        col,
        second_row,
    ));

    assert_eq!(
        (selected, h.stoat.diagnostics_picker.is_some()),
        (2, false),
        "the first press selects the clicked row and the second acts on it"
    );
}

/// A press on a continuation line of a wrapped result selects that result,
/// and a press on the first row under it changes nothing.
///
/// `a.rs` takes the list's first row. The long file under it wraps onto
/// `h` rows, so the list's third row is one of its continuation lines.
#[test]
fn a_press_on_a_continuation_line_selects_its_result() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 30);
    let root = PathBuf::from("/wrapped-press");
    h.fake_fs().insert_file(root.join("a.rs"), b"a\n");
    let long = format!("d/{}.rs", "x".repeat(300));
    h.fake_fs().insert_file(root.join(long), b"x\n");
    h.stoat.active_workspace_mut().git_root = root;
    h.type_keys("space p");
    h.settle();
    h.stoat.render();

    let list = boxed_modal_surfaces(&h.stoat)
        .expect("the finder is open")
        .list;
    let width = stoat::render::picker::label_columns(list.width);
    let rows = 1 + 303usize.div_ceil(width - 4) as u16;
    let selected = |h: &stoat::test_harness::TestHarness| {
        let finder = h.stoat.file_finder.as_ref().expect("the finder is open");
        finder.active_core_ref().picklist.selected
    };
    let before = selected(&h);
    left_press(&mut h, list.x + 1, list.y + 2);
    let on_continuation = selected(&h);
    left_press(&mut h, list.x + 1, list.y + 1 + rows);
    let under = selected(&h);

    assert_eq!((before, on_continuation, under), (0, 1, 1));
}

/// A press on a continuation line of the palette's argument list selects
/// the result that wraps there, as the finder's list does.
#[test]
fn a_press_on_an_argument_continuation_line_selects_its_result() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 30);
    let root = PathBuf::from("/wrapped-arg-press");
    h.fake_fs().insert_file(root.join("a.rs"), b"a\n");
    let long = format!("d/{}.rs", "x".repeat(300));
    h.fake_fs().insert_file(root.join(long), b"x\n");
    h.stoat.active_workspace_mut().git_root = root;
    h.type_text(":o ");
    h.snapshot();

    let list = boxed_modal_surfaces(&h.stoat)
        .expect("the palette is open")
        .list;
    let selected = |h: &stoat::test_harness::TestHarness| {
        let palette = h
            .stoat
            .command_palette
            .as_ref()
            .expect("the palette is open");
        let picker = palette.arg_picker.as_ref().expect("the argument picker");
        picker.active_core_ref().picklist.selected
    };
    let before = selected(&h);
    left_press(&mut h, list.x + 1, list.y + 2);

    assert_eq!((before, selected(&h)), (0, 1));
}

/// A press on a row of a pick list selects that row, as a press on any
/// list does.
#[test]
fn a_press_on_a_pick_row_selects_it() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 30);
    h.stoat.reload_user_config(
        r#"command peek { arg f = pick("ls"); run = shell("cat {f}"); output = notify; }"#,
    );
    h.fake_shell.set_response(
        "ls",
        stoat::host::ShellOutput {
            stdout: b"a\nb\nc\n".to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        },
    );
    h.type_text(":peek ");
    let _ = h.snapshot();
    h.settle();
    let _ = h.snapshot();

    let list = boxed_modal_surfaces(&h.stoat)
        .expect("the palette is open")
        .list;
    left_press(&mut h, list.x + 1, list.y + 1);
    let selected = h
        .stoat
        .command_palette
        .as_ref()
        .and_then(|palette| palette.pick.as_ref())
        .map(|pick| pick.list.selected);
    assert_eq!(selected, Some(1));
}

/// A press on a pixel pick list names the row under the pointer's pixel,
/// which the palette's pool reports. In list row 1, pixel row 22 is row 1,
/// and pixel row 20, the gap under row 0, is row 0. A plain press on that
/// cell takes its middle, pixel row 27, which is row 1.
#[test]
fn a_press_on_a_pixel_pick_row_selects_the_row_under_the_pointer() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 30);
    h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
    h.stoat.cell_pixels = Some((9, 18));
    h.stoat.display_scale = Some(1.0);
    h.stoat.reload_user_config(
        r#"command peek { arg f = pick("ls"); run = shell("cat {f}"); output = notify; }"#,
    );
    h.fake_shell.set_response(
        "ls",
        stoat::host::ShellOutput {
            stdout: b"a\nb\nc\nd\ne\n".to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        },
    );
    h.type_text(":peek ");
    let _ = h.snapshot();
    h.settle();
    let _ = h.snapshot();
    let list = boxed_modal_surfaces(&h.stoat)
        .expect("the palette is open")
        .list;

    let mut pressed = |report: Option<u16>| {
        let pick = h
            .stoat
            .command_palette
            .as_mut()
            .and_then(|palette| palette.pick.as_mut())
            .expect("a pick list");
        pick.list.selected = 4;
        match report {
            Some(pixel_row) => {
                let press = WindowIpcEvent::PoolMouse {
                    pool: stoat::smooth_scroll::non_pane_pool::PALETTE,
                    kind: MouseKind::Press(IpcMouseButton::Left),
                    col: list.x + 1,
                    row: list.y + 1,
                    cell_col: 9,
                    cell_row: pixel_row,
                    mods: 0,
                };
                stoat::app::deliver_window_event(&mut h.stoat, press);
            },
            None => left_press(&mut h, list.x + 1, list.y + 1),
        }
        h.stoat
            .command_palette
            .as_ref()
            .and_then(|palette| palette.pick.as_ref())
            .map(|pick| pick.list.selected)
    };

    assert_eq!(
        [pressed(Some(22)), pressed(Some(20)), pressed(None)],
        [Some(1), Some(0), Some(1)]
    );
}

/// A press on a pixel pick list in the strip under the last row whose box
/// fits names no row, since the list draws none there, and the selection
/// stays.
#[test]
fn a_press_under_the_last_pixel_pick_row_names_no_row() {
    let rows: String = (0..40).map(|at| format!("r{at}\n")).collect();
    let mut h = stoat::test_harness::TestHarness::with_size(120, 30);
    h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
    h.stoat.cell_pixels = Some((9, 18));
    h.stoat.display_scale = Some(1.0);
    h.stoat.reload_user_config(
        r#"command peek { arg f = pick("ls"); run = shell("cat {f}"); output = notify; }"#,
    );
    h.fake_shell.set_response(
        "ls",
        stoat::host::ShellOutput {
            stdout: rows.into_bytes(),
            stderr: Vec::new(),
            exit_code: 0,
        },
    );
    h.type_text(":peek ");
    let _ = h.snapshot();
    h.settle();
    let _ = h.snapshot();
    let list = boxed_modal_surfaces(&h.stoat)
        .expect("the palette is open")
        .list;
    let metrics = h
        .stoat
        .command_palette
        .as_ref()
        .and_then(|palette| palette.row_metrics)
        .expect("the list lays its rows out by pixels");
    let under = metrics.fit(list.height) as u32 * metrics.pitch(1);
    assert!(
        under < u32::from(list.height) * 18,
        "the list holds a strip under its rows"
    );

    let press = WindowIpcEvent::PoolMouse {
        pool: stoat::smooth_scroll::non_pane_pool::PALETTE,
        kind: MouseKind::Press(IpcMouseButton::Left),
        col: list.x + 1,
        row: list.y + (under / 18) as u16,
        cell_col: 9,
        cell_row: under as u16,
        mods: 0,
    };
    stoat::app::deliver_window_event(&mut h.stoat, press);
    let selected = h
        .stoat
        .command_palette
        .as_ref()
        .and_then(|palette| palette.pick.as_ref())
        .map(|pick| pick.list.selected);
    assert_eq!(selected, Some(0));
}

/// A press on a pixel list names the result under the middle pixel row of
/// the cell it lands on. The middles of cell rows 2, 3, and 4 are pixels
/// 45, 63, and 81, which the 21 px rows of results 2, 3, and 3 hold.
#[test]
fn a_press_on_a_pixel_list_selects_the_row_under_the_middle_of_its_cell() {
    let mut h = stoat::test_fixture::pixel_finder(5);
    let list = stoat::test_fixture::finder_layout(&h).list;
    let mut press = |row: u16| {
        let finder = h.stoat.file_finder.as_mut().expect("the finder is open");
        finder.active_core().picklist.selected = 0;
        left_press(&mut h, list.x + 1, list.y + row);
        let finder = h.stoat.file_finder.as_ref().expect("the finder is open");
        finder.active_core_ref().picklist.selected
    };

    assert_eq!([press(2), press(3), press(4)], [2, 3, 3]);
}

/// Press the left button in list row `row` of a pixel finder over 5 files,
/// as a `pool_mouse` report of pool `pool` at pixel row `pixel_row`, and
/// answer the selection after each press of `presses` in turn.
fn reported_presses(presses: &[(u32, u16, u16)]) -> Vec<usize> {
    let mut h = stoat::test_fixture::pixel_finder(5);
    let list = stoat::test_fixture::finder_layout(&h).list;
    presses
        .iter()
        .map(|&(pool, row, pixel_row)| {
            let press = WindowIpcEvent::PoolMouse {
                pool,
                kind: MouseKind::Press(IpcMouseButton::Left),
                col: list.x + 1,
                row: list.y + row,
                cell_col: 9,
                cell_row: pixel_row,
                mods: 0,
            };
            stoat::app::deliver_window_event(&mut h.stoat, press);
            let finder = h.stoat.file_finder.as_ref().expect("the finder is open");
            finder.active_core_ref().picklist.selected
        })
        .collect()
}

/// A press in list row 1 lands on the pixel row its pool reports, where the
/// row's middle, pixel 27, is result 1. Pixel row 22 is result 1, and pixel
/// row 20, the gap under result 0, is result 0.
#[test]
fn a_press_lands_on_the_pixel_row_under_the_pointer() {
    let finder = stoat::smooth_scroll::non_pane_pool::FINDER;

    assert_eq!(
        reported_presses(&[(finder, 1, 22), (finder, 1, 20)]),
        [1, 0]
    );
}

/// A report of another pool carries no pixel of the list, so a press in
/// list row 4 takes the row's middle, pixel 81, which is result 3, where
/// the reported pixel row 20 would be result 0.
#[test]
fn a_press_with_no_pixel_report_takes_the_middle_of_its_cell() {
    assert_eq!(reported_presses(&[(9, 4, 20)]), [3]);
}

/// A press on the palette's argument list lands on the pixel row its pool
/// reports, as a press on the finder's list does. In list row 1, pixel row
/// 22 is result 1 and pixel row 20 is result 0.
#[test]
fn a_press_on_an_argument_list_lands_on_the_pixel_row_under_the_pointer() {
    let mut h = stoat::test_harness::TestHarness::default();
    h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
    h.stoat.cell_pixels = Some((9, 18));
    h.stoat.display_scale = Some(1.0);
    let root = PathBuf::from("/pixel-argument-press");
    for n in 0..5 {
        h.fake_fs()
            .insert_file(root.join(format!("f{n:02}.rs")), b"x\n");
    }
    h.stoat.active_workspace_mut().git_root = root;
    h.type_text(":o ");
    h.snapshot();
    let list = stoat::test_fixture::palette_arg_layout(&h).list;

    let mut press = |pixel_row| {
        let press = WindowIpcEvent::PoolMouse {
            pool: stoat::smooth_scroll::non_pane_pool::PALETTE,
            kind: MouseKind::Press(IpcMouseButton::Left),
            col: list.x + 1,
            row: list.y + 1,
            cell_col: 9,
            cell_row: pixel_row,
            mods: 0,
        };
        stoat::app::deliver_window_event(&mut h.stoat, press);
        let palette = h
            .stoat
            .command_palette
            .as_ref()
            .expect("the palette is open");
        let picker = palette.arg_picker.as_ref().expect("the argument picker");
        picker.active_core_ref().picklist.selected
    };

    assert_eq!([press(22), press(20)], [1, 0]);
}

/// A press on the palette's command list lands on the pixel row its pool
/// reports, as a press on the argument list does. In list row 1, pixel
/// row 22 is row 1, and pixel row 20, the gap under row 0, is row 0.
#[test]
fn a_press_on_the_command_list_lands_on_the_pixel_row_under_the_pointer() {
    let mut h = stoat::test_harness::TestHarness::default();
    h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
    h.stoat.cell_pixels = Some((9, 18));
    h.stoat.display_scale = Some(1.0);
    h.type_text(":");
    h.snapshot();
    let list = boxed_modal_surfaces(&h.stoat)
        .expect("the palette is open")
        .list;

    let mut press = |pixel_row| {
        let press = WindowIpcEvent::PoolMouse {
            pool: stoat::smooth_scroll::non_pane_pool::PALETTE,
            kind: MouseKind::Press(IpcMouseButton::Left),
            col: list.x + 1,
            row: list.y + 1,
            cell_col: 9,
            cell_row: pixel_row,
            mods: 0,
        };
        stoat::app::deliver_window_event(&mut h.stoat, press);
        h.stoat
            .command_palette
            .as_ref()
            .expect("the palette is open")
            .selected
    };

    assert_eq!([press(22), press(20)], [1, 0]);
}
