use lsp_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag, Position, Range};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier},
};
use std::path::PathBuf;
use stoat::{
    action_handlers,
    action_handlers::dispatch,
    auto_reload::ReloadFlash,
    buffer::BufferId,
    diff_map::{DiffHunk, DiffHunkStatus, DiffMap, MarkState, StagedMark, TokenDetail},
    render::{
        editor::RowSeverity,
        review,
        review::{DiffColumns, DiffDials, DiffLayout},
        undercurl::UnderlineKind,
    },
    theme::scope as s,
    Stoat,
};
use stoat_action::{Conflict, ExtendToLineEnd, MoveDown, MoveRight, OpenFile, OpenFileFinder};
use stoat_config::{LineNumbers, WrapMode};
use stoat_text::{Bias, Point, SelectionGoal};
use stoatty_protocol::command::{decode_stream, Command};

fn diag(line: u32, severity: DiagnosticSeverity) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position { line, character: 0 },
            end: Position { line, character: 1 },
        },
        severity: Some(severity),
        message: String::new(),
        ..Default::default()
    }
}

/// The severity map is keyed by buffer row and derived from spans shifted
/// through the edits since each diagnostic was published, so typing above
/// one moves its row without the server having said anything.
#[test]
fn a_diagnostic_row_follows_an_edit_above_it_without_a_republish() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/severity-shift");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    h.seed_diagnostics(&path, vec![diag(1, DiagnosticSeverity::WARNING)]);

    let severity_rows = |h: &mut stoat::test_harness::TestHarness| -> Vec<u32> {
        let _ = h.stoat.render();
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        editor
            .gutter_severity_cache
            .as_ref()
            .expect("built by the paint")
            .map
            .rows_in(0..u32::MAX)
    };

    assert_eq!(
        severity_rows(&mut h),
        vec![1],
        "the row it was published on"
    );

    let buffer_id = action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .buffer_id;
    h.stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("buffer")
        .write()
        .expect("poisoned")
        .edit(0..0, "inserted\n");

    assert_eq!(
        severity_rows(&mut h),
        vec![2],
        "and the mark rides the inserted line down with its text",
    );
}

/// The severity map covers the whole buffer, so a window that starts below
/// a wrapped line still marks a diagnostic in view.
#[test]
fn the_severity_map_covers_the_whole_buffer() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/severity-wrap");
    let path = root.join("a.txt");
    let text = format!("{}\n{}", "a".repeat(400), "x\n".repeat(30));
    h.fake_fs().insert_file(&path, text.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    h.seed_diagnostics(&path, vec![diag(2, DiagnosticSeverity::ERROR)]);
    let _ = h.stoat.render();

    let severity_rows_from = |h: &mut stoat::test_harness::TestHarness, scroll_row: u32| {
        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .scroll_row = scroll_row;
        let _ = h.stoat.render();
        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .gutter_severity_cache
            .as_ref()
            .expect("built by the paint")
            .map
            .rows_in(0..u32::MAX)
    };
    let below_the_wrap = action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .display_map
        .snapshot()
        .buffer_to_display(Point::new(1, 0))
        .row;

    assert_eq!(
        (
            below_the_wrap > 1,
            severity_rows_from(&mut h, below_the_wrap),
            severity_rows_from(&mut h, 0)
        ),
        (true, vec![2], vec![2]),
        "the wrap moves buffer row 1 down, and the map names row 2 at either scroll",
    );
}

/// The rows are derived once and reused until something that could move
/// them does.
///
/// Deriving them resolves every visible display row down the layer stack,
/// and the gutter asks on every repaint, so reuse is the whole point.
/// Each input is moved on its own because one missing from the key leaves
/// the gutter marking where a diagnostic used to be.
#[test]
fn the_diagnostic_rows_are_reused_until_an_input_moves() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/severity-cache");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"one\ntwo\nthree\nfour\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    let snapshot = editor.display_map.snapshot();
    let severity = RowSeverity::for_rows([(2, DiagnosticSeverity::ERROR)]);
    let area = Rect::new(0, 0, 4, 12);

    let mut cache = None;
    let rows =
        stoat::render::editor::diagnostic_rows(&snapshot, &severity, 0, area, 4, 0, &mut cache)
            .to_vec();
    assert_eq!(
        rows,
        vec![(2u16, DiagnosticSeverity::ERROR)],
        "the diagnostic's row carries its severity",
    );

    // A severity this text cannot produce marks the held entry, so reuse is
    // told apart from a rebuild that lands on the same answer.
    cache.as_mut().expect("built").rows[0].1 = DiagnosticSeverity::HINT;
    assert_eq!(
        stoat::render::editor::diagnostic_rows(&snapshot, &severity, 0, area, 4, 0, &mut cache)[0]
            .1,
        DiagnosticSeverity::HINT,
        "a repeat frame rebuilt the rows instead of reusing them",
    );

    for (label, scroll, rect, end, version) in [
        ("a scroll", 1u32, area, 4u32, 0u64),
        ("a resize", 0, Rect::new(0, 0, 5, 12), 4, 0),
        ("a shorter viewport", 0, area, 3, 0),
        ("a new diagnostic set", 0, area, 4, 1),
    ] {
        // From the same baseline each time, or one case's key change would
        // stand in for the next's and an input missing from the key would
        // go unnoticed.
        let mut cache = None;
        stoat::render::editor::diagnostic_rows(&snapshot, &severity, 0, area, 4, 0, &mut cache);
        cache.as_mut().expect("built").rows[0].1 = DiagnosticSeverity::HINT;

        assert_eq!(
            stoat::render::editor::diagnostic_rows(
                &snapshot, &severity, scroll, rect, end, version, &mut cache
            )
            .first()
            .map(|&(_, sev)| sev),
            Some(DiagnosticSeverity::ERROR),
            "{label} must derive the rows again",
        );
    }
}

/// Severity is recorded per buffer row, and a wrapped line puts more display
/// rows on screen than the buffer has, so the mark has to be painted at the
/// display row its buffer row occupies.
#[test]
fn the_glyph_severity_gutter_marks_the_diagnostics_own_display_row() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/severity-wrap");
    let path = root.join("a.txt");
    h.fake_fs()
        .insert_file(&path, b"a long first line that wraps\nsecond\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    editor.display_map.set_wrap_width(Some(8));
    let snapshot = editor.display_map.snapshot();

    let wrapped_row = snapshot.buffer_to_display(Point::new(1, 0)).row;
    assert!(
        wrapped_row > 1,
        "the first line has to wrap for this to bite"
    );

    let severity = RowSeverity::for_rows([(1, DiagnosticSeverity::ERROR)]);
    let mut buf = Buffer::empty(Rect::new(0, 0, 4, 12));
    let theme = h.stoat.theme.clone();
    let mut cache = None;
    let rows = stoat::render::editor::diagnostic_rows(
        &snapshot,
        &severity,
        0,
        Rect::new(0, 0, 4, 12),
        snapshot.line_count().min(12),
        0,
        &mut cache,
    );
    stoat::render::editor::paint_diagnostic_gutter(rows, 0, 0, &theme, &mut buf);

    let marked: Vec<u32> = (0..12)
        .filter(|&y| buf[(0, y)].symbol() != " ")
        .map(u32::from)
        .collect();
    assert_eq!(
        marked,
        vec![wrapped_row],
        "the mark paints on the wrapped display row of buffer row 1",
    );
}

fn open_search_buffer(h: &mut stoat::test_harness::TestHarness, contents: &str) {
    let root = PathBuf::from("/search");
    let path = root.join("s.txt");
    h.fake_fs().insert_file(&path, contents.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
}

#[test]
fn inlay_hints_paint_in_the_virtual_style() {
    let mut h = Stoat::test();
    open_search_buffer(&mut h, "let x = 1");
    let theme = h.stoat.theme.clone();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let inlay_bg = theme
        .get(stoat::theme::scope::UI_VIRTUAL_INLAY)
        .bg
        .expect("the default theme sets an inlay bg");

    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    let inserts = {
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        vec![(
            buf_snap.anchor_at(5, Bias::Left),
            ": i32".to_string(),
            stoat::display_map::InlayKind::Hint,
        )]
    };
    editor.display_map.splice_inlays(Vec::new(), inserts);

    let area = Rect::new(0, 0, 40, 4);
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        fallback,
        &theme,
        &chrome,
        &mut buf,
        true,
        false,
        LineNumbers::Off,
        false,
        None,
        None,
        None,
        None,
        true,
        None,
        0.0,
        DiffDials::shipped(),
        WrapMode::None,
        80,
        0,
        &[],
        false,
        None,
        None,
        false,
    );

    let italic: String = (0..area.width)
        .filter(|&x| buf[(x, 0)].modifier.contains(Modifier::ITALIC))
        .map(|x| buf[(x, 0)].symbol())
        .collect();
    assert_eq!(
        italic, ": i32",
        "the inlay hint renders italic while code stays upright"
    );

    let hint_x = (0..area.width)
        .find(|&x| buf[(x, 0)].modifier.contains(Modifier::ITALIC))
        .expect("a hint cell exists");
    assert_eq!(
        buf[(hint_x, 0)].bg,
        inlay_bg,
        "the hint carries the inlay background wash"
    );
}

#[test]
fn selection_off_an_inlay_row_paints_the_same_as_without_inlays() {
    fn line1_row(with_inlay: bool) -> Vec<ratatui::buffer::Cell> {
        let mut h = Stoat::test();
        open_search_buffer(&mut h, "let x = 1\nlet y = 2");
        let theme = h.stoat.theme.clone();
        let fallback = theme.get(stoat::theme::scope::UI_TEXT);
        let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);

        if with_inlay {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            let inserts = {
                let snapshot = editor.display_map.snapshot();
                let buf_snap = snapshot.buffer_snapshot();
                vec![(
                    buf_snap.anchor_at(5, Bias::Left),
                    ": i32".to_string(),
                    stoat::display_map::InlayKind::Hint,
                )]
            };
            editor.display_map.splice_inlays(Vec::new(), inserts);
        }

        // Select line 1, which carries no inlay, so the row-scoped check
        // routes it through the fast paint path whether or not line 0 has an
        // inlay.
        dispatch(&mut h.stoat, &MoveDown);
        dispatch(&mut h.stoat, &ExtendToLineEnd);

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        stoat::render::editor::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut buf,
            true,
            false,
            LineNumbers::Off,
            false,
            None,
            None,
            None,
            None,
            true,
            None,
            0.0,
            DiffDials::shipped(),
            WrapMode::None,
            80,
            0,
            &[],
            false,
            None,
            None,
            false,
        );
        (0..area.width).map(|x| buf[(x, 1)].clone()).collect()
    }

    let sel_bg = {
        let h = Stoat::test();
        h.stoat
            .theme
            .get(stoat::theme::scope::UI_SELECTION_EDITOR)
            .bg
    };
    let with_inlay = line1_row(true);
    let without = line1_row(false);

    assert!(
        sel_bg.is_some() && with_inlay.iter().any(|c| Some(c.bg) == sel_bg),
        "the selection wash lands on line 1",
    );
    assert_eq!(
        with_inlay, without,
        "an inlay on line 0 must not change line 1's fast-path selection paint",
    );
}

/// Cursors the viewport cannot show contribute nothing to what it paints.
///
/// The pass skips them before converting each to a display point, and a
/// skip that dropped a cursor it should have drawn, or kept one it should
/// not have, shows up as a cell differing from the same frame painted with
/// only the on-screen cursors in the set.
#[test]
fn cursors_outside_the_viewport_paint_nothing() {
    // Scrolled well down a long buffer, so cursors can sit far above and far
    // below what is drawn.
    const ROWS: u16 = 6;
    const SCROLL: u32 = 40;

    fn painted(extra_rows: &[u32]) -> Buffer {
        let text: String = (0..120).map(|i| format!("line {i} of text\n")).collect();
        let mut h = Stoat::test();
        open_search_buffer(&mut h, &text);
        let theme = h.stoat.theme.clone();
        let fallback = theme.get(stoat::theme::scope::UI_TEXT);
        let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.scroll_row = SCROLL;

        // A cursor on the third drawn row, plus whatever else is asked for.
        let mut rows = vec![SCROLL + 2];
        rows.extend_from_slice(extra_rows);
        rows.sort_unstable();

        {
            let snapshot = editor.display_map.snapshot();
            let buf_snap = snapshot.buffer_snapshot();
            let rope = buf_snap.rope();
            let spans: Vec<(usize, usize)> = rows
                .iter()
                .map(|&row| {
                    let offset = rope.point_to_offset(Point::new(row, 0));
                    (offset, offset + 1)
                })
                .collect();
            editor
                .selections
                .replace_sorted_from_offsets(&spans, Bias::Right, buf_snap);
        }

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let area = Rect::new(0, 0, 40, ROWS);
        let mut buf = Buffer::empty(area);
        stoat::render::editor::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut buf,
            true,
            false,
            LineNumbers::Off,
            false,
            None,
            None,
            None,
            None,
            true,
            None,
            0.0,
            DiffDials::shipped(),
            WrapMode::None,
            80,
            0,
            &[],
            false,
            None,
            None,
            false,
        );
        buf
    }

    let only_visible = painted(&[]);
    let with_offscreen = painted(&[0, 3, SCROLL - 1, SCROLL + u32::from(ROWS), 119]);

    assert_eq!(
        only_visible, with_offscreen,
        "cursors above and below the drawn rows changed what was drawn",
    );
}

#[test]
fn measure_gutter_width_matches_the_painted_fallback_gutter() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/measure-gutter");
    let path = root.join("a.txt");
    let body: String = (0..120).map(|i| format!("line {i}\n")).collect();
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    rendered_gutter(&mut h.stoat, true, false, LineNumbers::Absolute, 6);

    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    let snapshot = editor.display_map.snapshot();
    assert_eq!(
        stoat::render::editor::gutter_width_digits(&snapshot),
        3,
        "120 lines need three digits",
    );
    assert_eq!(
        stoat::render::editor::measure_gutter_width(
            stoat::render::editor::gutter_width_digits(&snapshot),
            None
        ),
        editor.gutter_width,
        "the measured fallback width matches the painted gutter",
    );
}

/// The paint sizes the gutter from a buffer snapshot rather than a display
/// one, which is what lets it take a single snapshot per frame. That only
/// works if the buffer it reads is the current one, so an edit crossing a
/// digit boundary has to widen the gutter on the very next frame.
#[test]
fn the_gutter_widens_on_the_frame_an_edit_adds_a_digit() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/gutter-digits");
    let path = root.join("a.txt");
    let body: String = (0..98).map(|i| format!("line {i}\n")).collect();
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    rendered_gutter(&mut h.stoat, true, false, LineNumbers::Absolute, 6);
    let narrow = action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .gutter_width;

    // 98 lines of text plus the one the trailing newline opens is 99, which
    // still fits in two digits. One more line does not.
    let (_, buffer_id) = h.stoat.focused_editor_ids().expect("a focused editor");
    {
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let end = guard.snapshot.visible_text.len();
        guard.edit(end..end, "line 98\n");
    }

    rendered_gutter(&mut h.stoat, true, false, LineNumbers::Absolute, 6);
    let wide = action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .gutter_width;
    assert_eq!(
        wide,
        narrow + 1,
        "the hundredth line widens the gutter by its extra digit",
    );
}

fn open_wide_row(h: &mut stoat::test_harness::TestHarness, root: &str) {
    let root = PathBuf::from(root);
    let path = root.join("wide.txt");
    h.fake_fs()
        .insert_file(&path, "\u{6c49}\u{5b57}ab\n".as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
}

#[test]
fn wide_glyphs_advance_by_display_width() {
    let mut h = Stoat::test();
    open_wide_row(&mut h, "/cjk-glyphs");

    let theme = stoat::theme::Theme::empty();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    let area = Rect::new(0, 0, 20, 2);
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor(editor, area, fallback, &theme, None, &mut buf, true);

    let base = (0..area.width)
        .find(|&x| buf[(x, 0)].symbol() == "\u{6c49}")
        .expect("the first wide glyph is painted");
    assert_eq!(
        buf[(base + 1, 0)].symbol(),
        " ",
        "the wide glyph clears its trailing cell",
    );
    assert_eq!(buf[(base + 2, 0)].symbol(), "\u{5b57}");
    assert_eq!(buf[(base + 3, 0)].symbol(), " ");
    assert_eq!(buf[(base + 4, 0)].symbol(), "a");
    assert_eq!(buf[(base + 5, 0)].symbol(), "b");

    let snapshot = editor.display_map.snapshot();
    let a_point = snapshot.buffer_snapshot().rope().offset_to_point(6);
    assert_eq!(
        snapshot.buffer_to_display(a_point).column,
        4,
        "a is painted at the display column the width model reports",
    );
}

/// Render `content` as the only file and hand back the painted buffer.
fn painted_row(h: &mut stoat::test_harness::TestHarness, root: &str, content: &str) -> Buffer {
    let root = PathBuf::from(root);
    let path = root.join("marks.txt");
    h.fake_fs().insert_file(&path, content.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let theme = stoat::theme::Theme::empty();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    let area = Rect::new(0, 0, 20, 2);
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor(editor, area, fallback, &theme, None, &mut buf, true);
    buf
}

#[test]
fn a_combining_mark_paints_onto_the_character_it_sits_on() {
    let mut h = Stoat::test();
    let buf = painted_row(&mut h, "/marks-narrow", "ae\u{301}b\n");

    let base = (0..20u16)
        .find(|&x| buf[(x, 0u16)].symbol().starts_with('a'))
        .expect("the row is painted");
    assert_eq!(buf[(base, 0u16)].symbol(), "a");
    assert_eq!(
        buf[(base + 1, 0u16)].symbol(),
        "e\u{301}",
        "the accent rides in the cell holding its letter",
    );
    assert_eq!(buf[(base + 2, 0u16)].symbol(), "b");
}

#[test]
fn a_combining_mark_on_a_wide_character_paints_on_its_first_cell() {
    // The column has already advanced two cells past a wide character, so
    // the mark belongs two back rather than one, where the cleared second
    // half of the glyph sits.
    let mut h = Stoat::test();
    let buf = painted_row(&mut h, "/marks-wide", "a\u{6c49}\u{301}b\n");

    let base = (0..20u16)
        .find(|&x| buf[(x, 0u16)].symbol().starts_with('a'))
        .expect("the row is painted");
    assert_eq!(buf[(base + 1, 0u16)].symbol(), "\u{6c49}\u{301}");
    assert_eq!(
        buf[(base + 2, 0u16)].symbol(),
        " ",
        "the wide glyph's second cell stays cleared",
    );
    assert_eq!(buf[(base + 3, 0u16)].symbol(), "b");
}

#[test]
fn a_row_opening_with_a_combining_mark_drops_it() {
    // Nothing precedes the mark, so there is no cell for it to join. The
    // row above is longer, so a mark landing at the column that row ended
    // on would survive rather than being painted over by this one.
    let mut h = Stoat::test();
    let buf = painted_row(&mut h, "/marks-leading", "abcdef\n\u{301}x\n");

    let second_row: String = (0..20u16).map(|x| buf[(x, 1u16)].symbol()).collect();
    assert!(
        second_row.starts_with('x'),
        "the rest of the row still paints, got {second_row:?}",
    );
    assert!(
        !second_row.contains('\u{301}'),
        "and nothing carries over from the row above, got {second_row:?}",
    );
}

/// Render `content` in `theme` with the whole first line selected, and hand
/// back that row's cells.
fn selected_first_row(
    h: &mut stoat::test_harness::TestHarness,
    content: &str,
    theme: &stoat::theme::Theme,
) -> Vec<ratatui::buffer::Cell> {
    open_search_buffer(h, content);
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(theme);

    dispatch(&mut h.stoat, &ExtendToLineEnd);

    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    let area = Rect::new(0, 0, 40, 4);
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        fallback,
        theme,
        &chrome,
        &mut buf,
        true,
        false,
        LineNumbers::Off,
        false,
        None,
        None,
        None,
        None,
        true,
        None,
        0.0,
        DiffDials::shipped(),
        WrapMode::None,
        80,
        0,
        &[],
        false,
        None,
        None,
        false,
    );
    (0..area.width).map(|x| buf[(x, 0)].clone()).collect()
}

#[test]
fn a_selected_wide_glyph_is_washed_across_both_its_cells() {
    let mut h = Stoat::test();
    let theme = h.stoat.theme.clone();
    let row = selected_first_row(&mut h, "\u{6c49}\u{5b57}\u{4e00}\nx", &theme);
    let sel_bg = theme.get(stoat::theme::scope::UI_SELECTION_EDITOR).bg;
    assert!(sel_bg.is_some(), "the theme washes selections");

    // The wash skips the character the cursor sits on, which the line-end
    // extend leaves on the last glyph, so the first two are what it covers.
    let washed: Vec<bool> = (0..4).map(|x| Some(row[x].bg) == sel_bg).collect();
    assert_eq!(
        washed,
        vec![true; 4],
        "both cells of both glyphs, not every other one",
    );
}

#[test]
fn a_selected_tab_is_washed_across_the_cells_it_spans() {
    // A tab is not two cells wide, it is however many reach the next stop,
    // so the count comes from the column advance rather than the character.
    let mut h = Stoat::test();
    let theme = h.stoat.theme.clone();
    let row = selected_first_row(&mut h, "\tx\ny", &theme);
    let sel_bg = theme.get(stoat::theme::scope::UI_SELECTION_EDITOR).bg;

    // The x carries the cursor and so is not washed, leaving the tab's own
    // four cells as what the selection covers.
    let washed: Vec<bool> = (0..4).map(|x| Some(row[x].bg) == sel_bg).collect();
    assert_eq!(washed, vec![true; 4], "every cell the tab spans");
}

/// A theme that paints text in `text` under a `#636d83` selection wash.
fn wash_theme(text: &str) -> stoat::theme::Theme {
    let src =
        format!(r##"theme t {{ ui.text.fg = "{text}"; ui.selection.editor.bg = "#636d83"; }}"##);
    let (config, _) = stoat_config::parse(&src);
    stoat::theme::Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
}

/// Text in the wash's own color lifts toward white, the pole a dark wash is
/// furthest from, and stops at the floor. `#b4b9c4` stands
/// [`FAINT_CONTRAST_FLOOR`] above the `#636d83` wash in luma. The line-end
/// extend leaves the cursor on the `c`, so the wash covers the `a` and the
/// `b`.
///
/// [`FAINT_CONTRAST_FLOOR`]: stoat::render::paint::FAINT_CONTRAST_FLOOR
#[test]
fn a_selected_foreground_in_the_wash_color_lifts_to_the_floor() {
    let mut h = Stoat::test();
    let row = selected_first_row(&mut h, "abc\nx", &wash_theme("#636d83"));
    assert_eq!([row[0].fg, row[1].fg], [Color::Rgb(0xb4, 0xb9, 0xc4); 2]);
}

#[test]
fn a_selected_foreground_past_the_floor_keeps_its_color() {
    let mut h = Stoat::test();
    let row = selected_first_row(&mut h, "abc\nx", &wash_theme("#fafafa"));
    assert_eq!([row[0].fg, row[1].fg], [Color::Rgb(0xfa, 0xfa, 0xfa); 2]);
}

#[test]
fn a_selected_wide_glyph_on_an_inlay_row_is_washed_across_both_cells() {
    // An inlay on the row takes the paint off its fast path and onto the
    // one that recomputes each character's column, which has to widen the
    // wash the same way.
    let mut h = Stoat::test();
    open_search_buffer(&mut h, "\u{6c49}\u{5b57}x\ny");
    let theme = h.stoat.theme.clone();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let sel_bg = theme.get(stoat::theme::scope::UI_SELECTION_EDITOR).bg;

    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let inserts = {
            let snapshot = editor.display_map.snapshot();
            let buf_snap = snapshot.buffer_snapshot();
            vec![(
                buf_snap.anchor_at(6, Bias::Left),
                ": i32".to_string(),
                stoat::display_map::InlayKind::Hint,
            )]
        };
        editor.display_map.splice_inlays(Vec::new(), inserts);
    }
    dispatch(&mut h.stoat, &ExtendToLineEnd);

    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    let area = Rect::new(0, 0, 40, 4);
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        fallback,
        &theme,
        &chrome,
        &mut buf,
        true,
        false,
        LineNumbers::Off,
        false,
        None,
        None,
        None,
        None,
        true,
        None,
        0.0,
        DiffDials::shipped(),
        WrapMode::None,
        80,
        0,
        &[],
        false,
        None,
        None,
        false,
    );

    let washed: Vec<bool> = (0..4).map(|x| Some(buf[(x, 0)].bg) == sel_bg).collect();
    assert_eq!(washed, vec![true; 4], "both cells of both glyphs");
}

#[test]
fn a_cursor_on_a_wide_glyph_covers_both_its_cells() {
    let mut h = Stoat::test();
    open_search_buffer(&mut h, "\u{6c49}x\ny");
    let theme = stoat::theme::Theme::empty();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let cursor_modifier = theme.cursor_style().add_modifier;

    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    let area = Rect::new(0, 0, 40, 4);
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        fallback,
        &theme,
        &chrome,
        &mut buf,
        true,
        false,
        LineNumbers::Off,
        false,
        None,
        None,
        None,
        None,
        true,
        None,
        0.0,
        DiffDials::shipped(),
        WrapMode::None,
        80,
        0,
        &[],
        false,
        None,
        None,
        false,
    );

    assert!(
        !cursor_modifier.is_empty(),
        "the cursor style is distinguishable",
    );
    assert_eq!(
        (
            buf[(0u16, 0u16)].modifier.contains(cursor_modifier),
            buf[(1u16, 0u16)].modifier.contains(cursor_modifier),
        ),
        (true, true),
        "the block cursor covers the whole glyph it sits on",
    );
}

#[test]
fn the_cursor_lands_on_the_glyph_after_wide_chars() {
    let mut h = Stoat::test();
    open_wide_row(&mut h, "/cjk-cursor");
    dispatch(&mut h.stoat, &MoveRight);
    dispatch(&mut h.stoat, &MoveRight);
    let cursor_col = h.cursor_display_positions()[0].1;

    let theme = stoat::theme::Theme::empty();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    let area = Rect::new(0, 0, 20, 2);
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor(editor, area, fallback, &theme, None, &mut buf, true);

    let base = (0..area.width)
        .find(|&x| buf[(x, 0)].symbol() == "\u{6c49}")
        .expect("the first wide glyph is painted");
    let a_col = (0..area.width)
        .find(|&x| buf[(x, 0)].symbol() == "a")
        .expect("a is painted");
    assert_eq!(
        (a_col - base) as u32,
        cursor_col,
        "the a glyph is painted at the cursor's display column",
    );
}

/// Open a line many times wider than any test pane, carrying wide chars
/// past the right edge and spanning several rope chunks, followed by a
/// second line whose row proves the newline reset survives the skip.
fn open_over_wide_line(h: &mut stoat::test_harness::TestHarness, root: &str) {
    let root = PathBuf::from(root);
    let path = root.join("wide-line.txt");
    let line = format!(
        "{}{}{}",
        "x".repeat(10),
        "\u{6c49}".repeat(40),
        "y".repeat(30),
    );
    h.fake_fs()
        .insert_file(&path, format!("{line}\nsecond line\n").as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
}

fn row_text(buf: &Buffer, y: u16, width: u16) -> String {
    (0..width).map(|x| buf[(x, y)].symbol()).collect()
}

#[test]
fn an_over_wide_line_paints_its_visible_prefix_and_the_next_row() {
    let mut h = Stoat::test();
    open_over_wide_line(&mut h, "/over-wide");

    let theme = stoat::theme::Theme::empty();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    let area = Rect::new(0, 0, 20, 3);
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor(editor, area, fallback, &theme, None, &mut buf, true);

    assert_eq!(
        row_text(&buf, 0, area.width),
        "xxxxxxxxxx\u{6c49} \u{6c49} \u{6c49} \u{6c49} \u{6c49} ",
        "the over-wide line paints only the glyphs that fit",
    );
    assert_eq!(
        row_text(&buf, 1, area.width),
        "second line         ",
        "the newline resets the column after the skipped remainder",
    );
}

/// Open a single 200-column line with no trailing newline, so any wrapping
/// splits it across display rows.
fn open_long_line(h: &mut stoat::test_harness::TestHarness) {
    let root = PathBuf::from("/wrap");
    let path = root.join("long.txt");
    h.fake_fs().insert_file(&path, "a".repeat(200).as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
}

/// Render the focused editor with `wrap` and return its stamped wrap width
/// alongside the resulting display and buffer line counts.
fn wrap_after_render(
    stoat: &mut Stoat,
    area: Rect,
    wrap: WrapMode,
    wrap_column: u32,
) -> (Option<u32>, u32, u32) {
    let theme = stoat::theme::Theme::empty();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        fallback,
        &theme,
        &chrome,
        &mut buf,
        true,
        false,
        LineNumbers::Off,
        false,
        None,
        None,
        None,
        None,
        true,
        None,
        0.0,
        DiffDials::shipped(),
        wrap,
        wrap_column,
        0,
        &[],
        false,
        None,
        None,
        false,
    );
    let snapshot = editor.display_map.snapshot();
    (
        editor.display_map.wrap_width(),
        snapshot.line_count(),
        snapshot.buffer_line_count(),
    )
}

#[test]
fn editor_width_wrap_splits_a_long_line() {
    let mut h = Stoat::test();
    open_long_line(&mut h);
    let area = Rect::new(0, 0, 40, 10);
    let (width, display_rows, buffer_rows) =
        wrap_after_render(&mut h.stoat, area, WrapMode::EditorWidth, 80);
    assert_eq!(
        width,
        Some(39),
        "editor_width wraps one column short of the pane text width"
    );
    assert_eq!(buffer_rows, 1, "the buffer is one long line");
    assert_eq!(
        display_rows, 6,
        "200 columns wrap into six rows of up to 39"
    );
}

#[test]
fn wrap_override_forces_wrap_off_then_restores() {
    let mut h = Stoat::test();
    open_long_line(&mut h);
    let area = Rect::new(0, 0, 40, 10);

    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .wrap_override = Some(WrapMode::None);
    let (off_width, off_rows, buffer_rows) =
        wrap_after_render(&mut h.stoat, area, WrapMode::EditorWidth, 80);
    assert_eq!(
        off_width,
        Some(stoat::render::editor::MAX_UNWRAPPED_COLUMNS),
        "a wrap-off override wins over the editor_width frame",
    );
    assert_eq!(
        off_rows, buffer_rows,
        "the long line stays on its single row"
    );

    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .wrap_override = None;
    let (on_width, on_rows, _) = wrap_after_render(&mut h.stoat, area, WrapMode::EditorWidth, 80);
    assert_eq!(
        on_width,
        Some(39),
        "clearing the override follows the frame again"
    );
    assert!(
        on_rows > buffer_rows,
        "the line wraps once the override is cleared"
    );
}

#[test]
fn wrap_none_leaves_a_long_line_on_one_row() {
    let mut h = Stoat::test();
    open_long_line(&mut h);
    let area = Rect::new(0, 0, 40, 10);
    let (width, display_rows, buffer_rows) =
        wrap_after_render(&mut h.stoat, area, WrapMode::None, 80);
    assert_eq!(
        width,
        Some(stoat::render::editor::MAX_UNWRAPPED_COLUMNS),
        "none wraps only at the bound no pane reaches",
    );
    assert_eq!(
        display_rows, buffer_rows,
        "the long line keeps its single row and truncates",
    );
}

#[test]
fn wrap_none_still_breaks_a_line_past_the_bound() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/wrap");
    let path = root.join("huge.txt");
    let columns = stoat::render::editor::MAX_UNWRAPPED_COLUMNS as usize * 3;
    h.fake_fs()
        .insert_file(&path, "a".repeat(columns).as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let area = Rect::new(0, 0, 40, 10);
    let (_, display_rows, buffer_rows) = wrap_after_render(&mut h.stoat, area, WrapMode::None, 80);

    assert_eq!(buffer_rows, 1, "the buffer is one line");
    assert_eq!(
        display_rows, 4,
        "a line three times the bound spans four display rows, since each \
         continuation row holds four cells less than the bound",
    );
}

#[test]
fn bounded_wrap_caps_at_the_wrap_column() {
    let mut h = Stoat::test();
    open_long_line(&mut h);
    let area = Rect::new(0, 0, 40, 10);
    let (width, display_rows, _) = wrap_after_render(&mut h.stoat, area, WrapMode::Bounded, 20);
    assert_eq!(
        width,
        Some(20),
        "bounded caps at the wrap column below the pane width",
    );
    assert_eq!(
        display_rows, 13,
        "200 columns wrap into a 20-column row and twelve of 16 past the indent"
    );
}

#[test]
fn an_unrendered_editor_has_no_wrap_width() {
    let mut h = Stoat::test();
    open_long_line(&mut h);
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    assert_eq!(
        editor.display_map.wrap_width(),
        None,
        "an editor never rendered has no pane width to wrap at",
    );
}

#[test]
fn a_wrapped_continuation_row_paints_its_indent() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/wrap-indent");
    let path = root.join("a.txt");
    let body = format!("    {}", "word ".repeat(20));
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let theme = h.stoat.theme.clone();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let area = Rect::new(0, 0, 40, 10);
    let mut buf = Buffer::empty(area);
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        stoat::render::editor::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut buf,
            true,
            false,
            LineNumbers::Off,
            false,
            None,
            None,
            None,
            None,
            true,
            None,
            0.0,
            DiffDials::shipped(),
            WrapMode::EditorWidth,
            80,
            0,
            &[],
            false,
            None,
            None,
            false,
        );
    }

    let row_text = |y: u16| -> String {
        (0..area.width)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    };
    assert_eq!(
        row_text(1).trim_end(),
        format!("{}\u{21b3} {}", " ".repeat(6), ["word"; 6].join(" ")),
        "the Wrap Mark and a space close the indent, and the text starts 4 cells \
         past the line's 4-space indent",
    );
    assert_eq!(
        Some(buf[(6, 1)].fg),
        fallback
            .patch(stoat::render::editor::wrap_mark_style(&theme))
            .fg,
        "the mark paints in the wrap style",
    );
}

/// The shipped theme draws the Wrap Mark in its disabled color.
#[test]
fn the_wrap_mark_draws_in_the_disabled_color_under_ermine() {
    let mut h = stoat::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("wrap.txt", &format!("{}\n", "word ".repeat(20)));
    h.open_file(&path);
    h.snapshot();

    let mark = h
        .rendered_buffer()
        .content()
        .iter()
        .find(|cell| cell.symbol() == "\u{21b3}")
        .expect("a continuation row paints the Wrap Mark");
    assert_eq!(mark.fg, Color::Rgb(0x5b, 0x61, 0x6d));
}

#[test]
fn a_focused_pane_fills_every_row_of_its_cursor_line() {
    let (rows, text_x) = cursor_line_rows(&shipped_theme("ermine_night"), true);
    assert_eq!(
        rows,
        [
            "b".repeat(40),
            format!("{}ssc{}", "f".repeat(text_x), "f".repeat(37 - text_x)),
            "f".repeat(40),
            "b".repeat(40),
        ],
        "both rows of the cursor line fill across the gutter, and its selection keeps the wash",
    );
}

#[test]
fn an_unfocused_pane_fills_no_cursor_line() {
    let (rows, _) = cursor_line_rows(&shipped_theme("ermine_night"), false);
    assert_eq!(rows, vec!["b".repeat(40); 4]);
}

#[test]
fn a_theme_without_the_scope_fills_no_cursor_line() {
    let (rows, text_x) = cursor_line_rows(&shipped_theme("default_dark"), true);
    assert_eq!(
        rows,
        [
            "b".repeat(40),
            format!("{}ss{}", "b".repeat(text_x), "b".repeat(38 - text_x)),
            "b".repeat(40),
            "b".repeat(40),
        ],
        "no row fills, and the cursor reverses a base cell, so it reads as the base",
    );
}

/// The rich gutter boxes the cursor line's number in the fill, so the digits
/// stand on the fill as the rest of the row does.
#[test]
fn the_cursor_line_number_composites_over_the_fill() {
    let theme = shipped_theme("ermine_night");
    let mut h = stoat::test_harness::TestHarness::with_size(40, 8);
    let path = h.write_file("lines.txt", "one\ntwo\nthree\n");
    h.open_file(&path);
    h.type_keys("j");

    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let area = Rect::new(0, 0, 40, 4);
    let mut buf = Buffer::filled(area, stoat::render::surface_blank(&theme));
    let mut scene = stoat_widgets::ApcScene::new();
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        theme.get(stoat::theme::scope::UI_TEXT),
        &theme,
        &chrome,
        &mut buf,
        true,
        false,
        LineNumbers::Absolute,
        false,
        None,
        None,
        None,
        Some(&mut scene),
        true,
        None,
        0.0,
        DiffDials::shipped(),
        WrapMode::EditorWidth,
        80,
        0,
        &[],
        false,
        None,
        None,
        true,
    );

    let mut boxes: Vec<_> = decode_stream(scene.buffer())
        .into_iter()
        .filter_map(|command| match command {
            Command::TextRun(run) => Some((run.text, run.bg)),
            _ => None,
        })
        .collect();
    boxes.sort();
    let (gutter, fill) = (chrome.gutter_bg, Some([0x1b, 0x1e, 0x24]));
    assert_eq!(
        boxes,
        [
            ("1".to_string(), gutter),
            ("2".to_string(), fill),
            ("3".to_string(), gutter),
            ("4".to_string(), gutter),
        ],
        "only the cursor line's number takes the fill as its box",
    );
}

/// The theme `name` of the shipped config.
fn shipped_theme(name: &str) -> stoat::theme::Theme {
    let (config, _) = stoat_config::parse(stoat::app::DEFAULT_KEYMAP);
    stoat::theme::Theme::from_config(&config.expect("the shipped config parses"), name)
        .expect("the shipped theme builds")
}

/// The kind of each cell's background over the four rows of a 40-cell pane
/// that shows "above", a cursor line that wraps onto a second row, and
/// "below", painted under `theme`, with the column the text starts at.
///
/// The cursor line selects its first three cells. A cell reads `f` for the
/// cursor line fill, `b` for the base, `s` for the selection wash, `c` for
/// the cursor, and `?` for any other color.
fn cursor_line_rows(theme: &stoat::theme::Theme, focused: bool) -> (Vec<String>, usize) {
    let mut h = stoat::test_harness::TestHarness::with_size(40, 8);
    let text = format!("above\n{}\nbelow\n", "word ".repeat(10));
    let path = h.write_file("line.txt", &text);
    h.open_file(&path);
    h.type_keys("j v l l");

    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(theme);
    let area = Rect::new(0, 0, 40, 4);
    let base = stoat::render::surface_blank(theme);
    let mut buf = Buffer::filled(area, base.clone());
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        fallback,
        theme,
        &chrome,
        &mut buf,
        focused,
        false,
        LineNumbers::Absolute,
        false,
        None,
        None,
        None,
        None,
        true,
        None,
        0.0,
        DiffDials::shipped(),
        WrapMode::EditorWidth,
        80,
        0,
        &[],
        false,
        None,
        None,
        true,
    );

    let wash = theme.get(stoat::theme::scope::UI_SELECTION_EDITOR).bg;
    let cursor = theme.cursor_style().bg;
    let kind = |color: Color| match color {
        Color::Rgb(0x1b, 0x1e, 0x24) => 'f',
        color if color == base.bg => 'b',
        color if Some(color) == wash => 's',
        color if Some(color) == cursor => 'c',
        _ => '?',
    };
    let rows = (0..area.height)
        .map(|y| (0..area.width).map(|x| kind(buf[(x, y)].bg)).collect())
        .collect();
    (rows, editor.text_rect.expect("the text painted").x as usize)
}

/// Paint the focused editor's line-number gutter and return its
/// geometry-cache key.
fn paint_gutter_key(stoat: &mut Stoat, rows: u16, wrap: WrapMode) -> u64 {
    let theme = stoat::theme::Theme::empty();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
    let area = Rect::new(0, 0, 12, rows);
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        fallback,
        &theme,
        &chrome,
        &mut buf,
        true,
        false,
        LineNumbers::Absolute,
        false,
        None,
        None,
        None,
        None,
        true,
        None,
        0.0,
        DiffDials::shipped(),
        wrap,
        80,
        0,
        &[],
        false,
        None,
        None,
        false,
    );
    editor
        .gutter_geometry_cache
        .as_ref()
        .expect("gutter cache set")
        .geometry_key
}

/// The focused editor's cached folded gutter lines.
///
/// Clearing them is a rebuild sentinel. The next paint either reuses the
/// cache and leaves them empty, or rebuilds the geometry and repopulates
/// them. Their address is the same sentinel without the mutation, since a
/// rebuild allocates its replacement while the old vec is still cached.
fn cached_folded(stoat: &mut Stoat) -> &mut Vec<(u32, u16)> {
    &mut action_handlers::focused_editor_mut(stoat)
        .expect("focused editor")
        .gutter_geometry_cache
        .as_mut()
        .expect("gutter geometry cached")
        .folded
}

#[test]
fn gutter_geometry_cache_reuses_until_an_input_changes() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/gutter-cache");
    let path = root.join("a.txt");
    h.fake_fs()
        .insert_file(&path, b"one\ntwo\nthree\nfour\nfive");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let key = paint_gutter_key(&mut h.stoat, 5, WrapMode::None);
    cached_folded(&mut h.stoat).clear();

    assert_eq!(
        paint_gutter_key(&mut h.stoat, 5, WrapMode::None),
        key,
        "an identical paint keeps the cache key"
    );
    assert!(
        cached_folded(&mut h.stoat).is_empty(),
        "an identical paint reuses the cached geometry instead of rebuilding it"
    );

    action_handlers::focused_editor_mut(&mut h.stoat)
        .unwrap()
        .scroll_row = 1;

    assert_ne!(
        paint_gutter_key(&mut h.stoat, 5, WrapMode::None),
        key,
        "a scroll changes the cache key"
    );
    assert!(
        !cached_folded(&mut h.stoat).is_empty(),
        "an invalidated cache rebuilds the geometry"
    );

    // The pane keeps its width, so the wrap version is the only part of the
    // key this moves.
    let unwrapped = paint_gutter_key(&mut h.stoat, 5, WrapMode::None);
    assert_ne!(
        paint_gutter_key(&mut h.stoat, 5, WrapMode::EditorWidth),
        unwrapped,
        "turning wrapping on changes the cache key"
    );
}

/// A settled background rewrap is the one mapping change that announces
/// itself through no version the gutter used to watch.
///
/// The first wrap of a file at or above the deferral threshold runs off the
/// paint thread, so the paint that requested it draws the unwrapped row
/// mapping. Nothing about the buffer, the folds, or the diff moves when the
/// real wrapping lands, so a key blind to it leaves the line numbers
/// numbering rows the text no longer occupies.
#[test]
fn a_settled_background_rewrap_rebuilds_the_gutter_geometry() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/gutter-rewrap");
    let path = root.join("wide.txt");

    // Lines long enough to wrap several times in a 12-column pane, and more
    // of them than the threshold that sends the first wrap to the background.
    let body = std::iter::repeat_n("the quick brown fox jumps over the lazy dog", 120)
        .collect::<Vec<_>>()
        .join("\n");
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    /// The display rows the focused editor's mapping currently spans, which
    /// grows once the real wrapping replaces the interpolated pass-through.
    fn display_rows(stoat: &mut Stoat) -> u32 {
        action_handlers::focused_editor_mut(stoat)
            .unwrap()
            .display_map
            .snapshot()
            .max_point()
            .row
    }

    let deferred = paint_gutter_key(&mut h.stoat, 5, WrapMode::EditorWidth);
    let rows_deferred = display_rows(&mut h.stoat);
    cached_folded(&mut h.stoat).clear();

    h.settle();
    assert!(
        display_rows(&mut h.stoat) > rows_deferred,
        "the paint deferred its wrap, so the settle is what lands the wrapped rows",
    );

    assert_ne!(
        paint_gutter_key(&mut h.stoat, 5, WrapMode::EditorWidth),
        deferred,
        "the settled wrapping changes the cache key"
    );
    assert!(
        !cached_folded(&mut h.stoat).is_empty(),
        "so the gutter rebuilds its folded rows against the wrapped mapping"
    );
}

/// Paint the focused editor's rich sub-cell gutter with relative numbering
/// and return the number each cached component line carries.
///
/// A scene plus the shipped theme is what engages the rich path, where the
/// component lines are the artifact the relative numbers land in.
fn rich_gutter_numbers(stoat: &mut Stoat, rows: u16) -> Vec<u32> {
    let theme = stoat.theme.clone();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let mut scene = stoat_widgets::ApcScene::new();
    let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
    let area = Rect::new(0, 0, 12, rows);
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        fallback,
        &theme,
        &chrome,
        &mut buf,
        true,
        false,
        LineNumbers::Relative,
        false,
        None,
        None,
        None,
        Some(&mut scene),
        true,
        None,
        0.0,
        DiffDials::shipped(),
        WrapMode::None,
        80,
        0,
        &[],
        false,
        None,
        None,
        false,
    );
    editor
        .gutter_geometry_cache
        .as_ref()
        .expect("gutter cache set")
        .lines
        .iter()
        .map(|line| line.number)
        .collect()
}

/// Relative numbering is the default, so a cursor move changes the painted
/// numbers on a gutter whose geometry is untouched. Rebuilding the geometry
/// for it would re-resolve every diff hunk in the file per keypress.
#[test]
fn a_cursor_move_renumbers_the_gutter_lines_and_reuses_its_geometry() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/gutter-relnum-cache");
    let path = root.join("a.txt");
    h.fake_fs()
        .insert_file(&path, b"one\ntwo\nthree\nfour\nfive");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    assert_eq!(
        rich_gutter_numbers(&mut h.stoat, 5),
        [1, 1, 2, 3, 4],
        "the cursor's line keeps its absolute number, the rest their distance",
    );
    let geometry = cached_folded(&mut h.stoat).as_ptr();

    dispatch(&mut h.stoat, &MoveDown);

    assert_eq!(
        rich_gutter_numbers(&mut h.stoat, 5),
        [1, 2, 1, 2, 3],
        "the moved cursor renumbers every line",
    );
    assert_eq!(
        cached_folded(&mut h.stoat).as_ptr(),
        geometry,
        "renumbering reuses the cached geometry instead of resolving the hunks again",
    );
}

/// Paint the focused editor into `area` and return the APC frame it emits.
///
/// Read before any flush, so the decoration lane still holds the frame the
/// paint just built. Handed back as text because the frame is ASCII
/// throughout, and a mismatch between two of them reads as commands rather
/// than as a pair of byte arrays.
fn gutter_scene_frame(stoat: &mut Stoat, area: Rect) -> String {
    let theme = stoat.theme.clone();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let mut scene = stoat_widgets::ApcScene::new();
    let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        fallback,
        &theme,
        &chrome,
        &mut buf,
        true,
        false,
        LineNumbers::Relative,
        false,
        None,
        None,
        None,
        Some(&mut scene),
        true,
        None,
        0.0,
        DiffDials::shipped(),
        WrapMode::None,
        80,
        0,
        &[],
        false,
        None,
        None,
        false,
    );
    String::from_utf8_lossy(scene.bytes()).into_owned()
}

/// The same paint from an empty cache, which is the frame a full encode
/// writes with nothing to splice.
fn cold_gutter_scene_frame(stoat: &mut Stoat, area: Rect) -> String {
    action_handlers::focused_editor_mut(stoat)
        .expect("focused editor")
        .gutter_geometry_cache = None;
    gutter_scene_frame(stoat, area)
}

/// The memoized gutter frame is spliced whole, so it is only correct while
/// it matches what a full encode writes. Each input the splice is keyed on
/// is moved in turn, since a key that misses one paints the previous frame
/// at the new state.
#[test]
fn a_spliced_gutter_frame_matches_a_freshly_encoded_one() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/gutter-scene-memo");
    let path = root.join("a.txt");
    h.fake_fs()
        .insert_file(&path, b"one\ntwo\nthree\nfour\nfive");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let area = Rect::new(0, 0, 12, 5);
    gutter_scene_frame(&mut h.stoat, area);
    let spliced = gutter_scene_frame(&mut h.stoat, area);

    assert!(!spliced.is_empty(), "the rich gutter emits an APC frame");
    assert_eq!(
        spliced,
        cold_gutter_scene_frame(&mut h.stoat, area),
        "an unchanged repaint splices the frame a full encode writes",
    );

    dispatch(&mut h.stoat, &MoveDown);
    let moved = gutter_scene_frame(&mut h.stoat, area);

    assert_ne!(moved, spliced, "the moved cursor renumbers the gutter");
    assert_eq!(
        moved,
        cold_gutter_scene_frame(&mut h.stoat, area),
        "a cursor move re-encodes instead of splicing the numbers it cached",
    );

    let shifted = Rect::new(3, 1, 12, 5);
    let at_shifted = gutter_scene_frame(&mut h.stoat, shifted);

    assert_ne!(
        at_shifted, moved,
        "the frame is positioned against the rect"
    );
    assert_eq!(
        at_shifted,
        cold_gutter_scene_frame(&mut h.stoat, shifted),
        "a moved pane re-encodes instead of splicing the frame from its old rect",
    );
}

/// Render the focused editor's gutter into cells and return the trimmed
/// number string each visible row paints.
///
/// Passes no scene, so the gutter takes its cell fallback and the numbers
/// land as real glyphs the assertions can read.
fn rendered_gutter(
    stoat: &mut Stoat,
    is_focused: bool,
    insert_mode: bool,
    line_numbers: LineNumbers,
    rows: u16,
) -> Vec<String> {
    let area = Rect::new(0, 0, 12, rows);
    let buf = render_without_scene(stoat, area, is_focused, insert_mode, line_numbers);
    let gutter_w = action_handlers::focused_editor_mut(stoat)
        .expect("focused editor")
        .gutter_width;
    (0..rows)
        .map(|y| {
            (0..gutter_w)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim()
                .to_string()
        })
        .collect()
}

/// Render the focused editor's diff view 120 columns wide and return the
/// trimmed number its live column paints on each visible row.
fn rendered_diff_numbers(
    stoat: &mut Stoat,
    is_focused: bool,
    insert_mode: bool,
    rows: u16,
) -> Vec<String> {
    let area = Rect::new(0, 0, 120, rows);
    let buf = render_without_scene(stoat, area, is_focused, insert_mode, LineNumbers::Relative);
    let digits = review::diff_gutter_digits(
        &action_handlers::focused_editor_mut(stoat)
            .expect("focused editor")
            .display_map
            .snapshot(),
    );
    let x = DiffColumns::compute(area, DiffLayout::diff_view(digits, None)).right_num_x;
    (0..rows)
        .map(|y| {
            (x..x + digits)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim()
                .to_string()
        })
        .collect()
}

/// Render the focused editor into `area` with no scene and no minimap.
fn render_without_scene(
    stoat: &mut Stoat,
    area: Rect,
    is_focused: bool,
    insert_mode: bool,
    line_numbers: LineNumbers,
) -> Buffer {
    let theme = stoat::theme::Theme::empty();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        fallback,
        &theme,
        &chrome,
        &mut buf,
        is_focused,
        false,
        line_numbers,
        insert_mode,
        None,
        None,
        None,
        None,
        true,
        None,
        0.0,
        DiffDials::shipped(),
        WrapMode::None,
        80,
        0,
        &[],
        false,
        None,
        None,
        false,
    );
    buf
}

/// Render the focused editor at `width` x `rows` under the given minimap
/// flag, returning the recorded strip rect and, per row, the
/// symbols painted in the rightmost [`stoat::render::editor::MINIMAP_STRIP_COLS`] columns.
fn render_minimap(
    stoat: &mut Stoat,
    minimap_enabled: bool,
    width: u16,
    rows: u16,
) -> (Option<Rect>, Vec<String>) {
    let theme = stoat::theme::Theme::empty();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
    let area = Rect::new(0, 0, width, rows);
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        fallback,
        &theme,
        &chrome,
        &mut buf,
        true,
        minimap_enabled,
        LineNumbers::Off,
        false,
        None,
        None,
        None,
        None,
        true,
        None,
        0.0,
        DiffDials::shipped(),
        WrapMode::None,
        80,
        0,
        &[],
        false,
        None,
        None,
        false,
    );
    let rect = editor.minimap_rect;
    let cols = rect.map_or(width..width, |rect| rect.x..rect.x + rect.width);
    let strip = (0..rows)
        .map(|y| {
            cols.clone()
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    (rect, strip)
}

#[test]
fn minimap_strip_reserves_right_edge_under_stoatty() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/minimap");
    let path = root.join("a.txt");
    let line = "x".repeat(100);
    let body = format!("{line}\n{line}\n{line}");
    h.fake_fs().insert_file(&path, body.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    let (rect, strip) = render_minimap(&mut h.stoat, true, 120, 3);
    assert_eq!(
        rect,
        Some(Rect::new(
            113,
            0,
            stoat::render::editor::MINIMAP_STRIP_COLS,
            3
        )),
        "strip pins to the right edge at full width"
    );
    assert!(
        strip.iter().all(|row| row.chars().all(|c| c == ' ')),
        "text never paints into the reserved strip: {strip:?}"
    );
}

/// A diff view reserves the minimap strip at the right edge, as a plain
/// editor does.
#[test]
fn diff_view_reserves_the_minimap_strip() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/minimap-diff");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"one\ntwo\nthree");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .set_diff_view(true);

    let (rect, strip) = render_minimap(&mut h.stoat, true, 120, 3);
    let text_end = action_handlers::focused_editor_mut(&mut h.stoat)
        .and_then(|editor| editor.text_rect)
        .map(|text| text.x + text.width);
    assert_eq!(
        (rect, text_end),
        (
            Some(Rect::new(
                113,
                0,
                stoat::render::editor::MINIMAP_STRIP_COLS,
                3
            )),
            Some(113)
        ),
        "the strip pins to the right edge, and the live column ends where it starts"
    );
    assert!(
        strip.iter().all(|row| row.chars().all(|c| c == ' ')),
        "the diff paint never reaches the reserved strip: {strip:?}"
    );
}

/// A conflict view reserves the minimap strip at the right edge, as a diff
/// view does, so its grid ends where the strip starts.
#[test]
fn the_conflict_view_reserves_the_minimap_strip() {
    let mut h = Stoat::test();
    let git_root = h.stoat.active_workspace().git_root.clone();
    h.fake_git().add_repo(git_root).conflicted_file(
        "f.txt",
        Some("base\n"),
        Some("ours\n"),
        Some("theirs\n"),
    );
    dispatch(&mut h.stoat, &Conflict);
    h.settle();

    let (rect, strip) = render_minimap(&mut h.stoat, true, 120, 3);
    assert_eq!(
        rect,
        Some(Rect::new(
            113,
            0,
            stoat::render::editor::MINIMAP_STRIP_COLS,
            3
        )),
        "the strip pins to the right edge"
    );
    assert!(
        strip.iter().all(|row| row.chars().all(|c| c == ' ')),
        "the conflict paint never reaches the reserved strip: {strip:?}"
    );
}

#[test]
fn minimap_strip_absent_when_disabled_or_narrow() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/minimap-off");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"one\ntwo\nthree");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    assert_eq!(
        render_minimap(&mut h.stoat, false, 120, 3).0,
        None,
        "no strip when the minimap is disabled"
    );
    assert_eq!(
        render_minimap(&mut h.stoat, true, 107, 3).0,
        None,
        "no strip one column below the minimum pane width"
    );
}

#[test]
fn relative_line_numbers_center_on_the_cursor_line() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/relnum");
    let path = root.join("a.txt");
    h.fake_fs()
        .insert_file(&path, b"one\ntwo\nthree\nfour\nfive");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    dispatch(&mut h.stoat, &MoveDown);
    dispatch(&mut h.stoat, &MoveDown);

    // When focused and in normal mode, the cursor's line keeps its absolute
    // number (3) and every other line shows its distance from it.
    assert_eq!(
        rendered_gutter(&mut h.stoat, true, false, LineNumbers::Relative, 5),
        ["2", "1", "3", "1", "2"],
    );
}

/// Render the focused editor's gutter in fallback mode and return each
/// visible row's `(git glyph, glyph color)`, read from the glyph cell
/// right of the number. Uses the active theme, so each change kind resolves
/// to its own color.
fn gutter_mark_cells(stoat: &mut Stoat, rows: u16) -> Vec<(String, Color)> {
    let theme = stoat.theme.clone();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
    let area = Rect::new(0, 0, 12, rows);
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        fallback,
        &theme,
        &chrome,
        &mut buf,
        true,
        false,
        LineNumbers::Absolute,
        false,
        None,
        None,
        None,
        None,
        true,
        None,
        0.0,
        DiffDials::shipped(),
        WrapMode::None,
        80,
        0,
        &[],
        false,
        None,
        None,
        false,
    );
    let glyph_x = editor.gutter_width - 2;
    (0..rows)
        .map(|y| (buf[(glyph_x, y)].symbol().to_string(), buf[(glyph_x, y)].fg))
        .collect()
}

/// The plain gutter carries no staged state, so a staged and an unstaged
/// change paint the same glyph in the same color.
#[test]
fn gutter_marks_staged_and_unstaged_lines_alike() {
    let mut h = Stoat::test();
    h.stage_index_scenario(
        "/repo",
        &[("f.txt", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n")],
    );
    h.stoat.set_diff_warm_auto(true);
    h.open_file(std::path::Path::new("/repo/f.txt"));
    h.settle_diff_jobs();

    let cells = gutter_mark_cells(&mut h.stoat, 6);
    let modified = h
        .stoat
        .theme
        .get(s::DIFF_MODIFIED)
        .fg
        .expect("the theme sets it");
    let marked = ("▎".to_string(), modified);
    // Line 2 (b -> B) is staged in the index; line 4 (d -> D) is unstaged.
    assert_eq!(
        [cells[1].clone(), cells[3].clone()],
        [marked.clone(), marked],
        "{cells:?}",
    );
}

#[test]
fn gutter_marks_a_deletion_seam() {
    let mut h = Stoat::test();
    h.stage_index_scenario(
        "/repo",
        &[("f.txt", "a\nb\nc\nd\n", "a\nb\nc\nd\n", "a\nb\nd\n")],
    );
    h.stoat.set_diff_warm_auto(true);
    h.open_file(std::path::Path::new("/repo/f.txt"));
    h.settle_diff_jobs();

    let cells = gutter_mark_cells(&mut h.stoat, 6);
    assert!(
        cells.iter().any(|(mark, _)| mark == "▔"),
        "the row below the deleted line carries the seam mark: {cells:?}",
    );
}

/// A harness with `text` open from a directory no repo holds, so no diff
/// job replaces a map that [`install_diff_map`] puts in place.
fn open_outside_a_repo(text: &str) -> stoat::test_harness::TestHarness {
    let mut h = Stoat::test();
    let root = PathBuf::from("/no-repo");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, text.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    h
}

/// Install `dm` as the focused buffer's diff map.
fn install_diff_map(stoat: &mut Stoat, dm: DiffMap) {
    let buffer_id = action_handlers::focused_editor_mut(stoat)
        .expect("focused editor")
        .buffer_id;
    stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("buffer")
        .write()
        .expect("poisoned")
        .diff_map = Some(dm);
}

/// A staged mark is a change the index holds with no hunk over it, and its
/// row takes the git glyph in its kind's color.
#[test]
fn gutter_marks_a_staged_change_with_no_hunk() {
    let mut h = open_outside_a_repo("a\nb\nc");
    let mut dm = DiffMap::from_hunks([], Some(String::from("a\nb\nc").into()));
    dm.set_staged_marks(vec![StagedMark {
        status: DiffHunkStatus::Modified,
        base_lines: 1..2,
    }]);
    install_diff_map(&mut h.stoat, dm);

    let theme = h.stoat.theme.clone();
    let color = |scope: &str| theme.get(scope).fg.expect("the theme sets it");
    let cells = gutter_mark_cells(&mut h.stoat, 3);
    assert_eq!(
        cells[1],
        ("▎".to_string(), color(s::DIFF_MODIFIED)),
        "row 1 takes the git glyph in the modified color: {cells:?}",
    );
    assert_eq!(
        cells
            .iter()
            .map(|(glyph, _)| glyph.as_str())
            .collect::<Vec<_>>(),
        [" ", "▎", " "],
        "and no other row takes a mark",
    );
}

/// The plain gutter marks every row a hunk covers, as git counts the whole
/// line changed, whatever the tree pass found on it.
#[test]
fn gutter_marks_every_row_of_every_hunk() {
    let mut h = open_outside_a_repo("a\n    b\n    c\nd\ne\n");
    let reached = DiffHunk {
        status: DiffHunkStatus::Modified,
        buffer_start_line: 1,
        buffer_line_range: 1..3,
        base_byte_range: 2..6,
        anchor_range: None,
        token_detail: Some(std::sync::Arc::new(TokenDetail {
            buffer_spans: Vec::new(),
            base_spans: Vec::new(),
            tree: true,
        })),
        unstaged_lines: std::iter::once(1..3).collect(),
        marked_rows: std::iter::once(1..2).collect(),
        relocated_rows: Vec::new(),
    };
    let unreached = DiffHunk {
        buffer_start_line: 4,
        buffer_line_range: 4..5,
        base_byte_range: 8..9,
        token_detail: None,
        unstaged_lines: std::iter::once(4..5).collect(),
        marked_rows: Vec::new(),
        relocated_rows: Vec::new(),
        ..reached.clone()
    };
    let base = String::from("a\nb\nc\nd\nE\n");
    install_diff_map(
        &mut h.stoat,
        DiffMap::from_hunks([reached, unreached], Some(base.into())),
    );

    assert_eq!(
        gutter_mark_cells(&mut h.stoat, 6)
            .iter()
            .map(|(glyph, _)| glyph.as_str())
            .collect::<Vec<_>>(),
        [" ", "▎", "▎", " ", "▎", " "],
        "both rows of the reached hunk and the unreached hunk's row take the git glyph",
    );
}

/// The plain editor's rich gutter draws two lanes per marked row, the
/// change kind's full color in the hunk lane and the hunk's state color in
/// the state lane beside it, on a row the tree pass reached and on one it
/// has not.
#[test]
fn the_rich_gutter_draws_the_hunk_and_state_lanes() {
    use stoatty_protocol::command::{encode_bar, BarCommand};

    let mut h = open_outside_a_repo("a\nB\nC\n");
    let reached = DiffHunk {
        status: DiffHunkStatus::Modified,
        buffer_start_line: 1,
        buffer_line_range: 1..2,
        base_byte_range: 2..4,
        anchor_range: None,
        token_detail: Some(std::sync::Arc::new(TokenDetail {
            buffer_spans: Vec::new(),
            base_spans: Vec::new(),
            tree: true,
        })),
        unstaged_lines: std::iter::once(1..2).collect(),
        marked_rows: std::iter::once(1..2).collect(),
        relocated_rows: Vec::new(),
    };
    let unreached = DiffHunk {
        buffer_start_line: 2,
        buffer_line_range: 2..3,
        base_byte_range: 4..6,
        token_detail: None,
        unstaged_lines: std::iter::once(2..3).collect(),
        marked_rows: Vec::new(),
        relocated_rows: Vec::new(),
        ..reached.clone()
    };
    let base = String::from("a\nb\nc\n");
    install_diff_map(
        &mut h.stoat,
        DiffMap::from_hunks([reached, unreached], Some(base.into())),
    );

    let theme = h.stoat.theme.clone();
    let color =
        |scope: &str| stoat::minimap::color_to_rgb(theme.get(scope).fg.expect("the theme sets it"));
    let frame = gutter_scene_frame(&mut h.stoat, Rect::new(0, 0, 12, 4));
    let bar = |x, y, color| {
        let command = encode_bar(&BarCommand {
            x,
            y,
            width: 6,
            height: 16,
            color,
            occludes: false,
            hatch: None,
            radius: 0,
            corners: 0,
        });
        frame.contains(String::from_utf8_lossy(&command).as_ref())
    };
    let (modified, unstaged) = (color(s::DIFF_MODIFIED), color(s::DIFF_UNSTAGED));
    // The hunk lane sits at the gutter's left edge, and the state lane a
    // 6/16 lane and a 1/16 gap right of it.
    assert_eq!(
        [
            bar(0, 16, modified),
            bar(7, 16, unstaged),
            bar(0, 32, modified),
            bar(7, 32, unstaged),
        ],
        [true; 4],
        "both lanes on each marked row, the unreached row's hunk bar at full color: {frame:?}",
    );
}

/// A theme that styles no approval scope paints approvals in its staged
/// scopes, so its rows keep two state colors under a review.
#[test]
fn approval_scopes_take_the_staged_scopes_in_a_theme_without_them() {
    let bare = {
        let (config, _) = stoat_config::parse(r##"theme t { diff.staged.fg = "#00ffff"; }"##);
        stoat::theme::Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    };
    let shipped = Stoat::test().stoat.theme.clone();
    let scopes = |theme: &stoat::theme::Theme| {
        [
            MarkState::Staged,
            MarkState::Unstaged,
            MarkState::Approved,
            MarkState::Unapproved,
        ]
        .map(|state| stoat::render::editor::mark_state_scope(theme, state))
    };

    assert_eq!(
        [scopes(&bare), scopes(&shipped)],
        [
            [
                s::DIFF_STAGED,
                s::DIFF_UNSTAGED,
                s::DIFF_STAGED,
                s::DIFF_UNSTAGED
            ],
            [
                s::DIFF_STAGED,
                s::DIFF_UNSTAGED,
                s::DIFF_APPROVED,
                s::DIFF_UNAPPROVED
            ],
        ],
    );
}

/// The gutter's mark colors carry the shipped theme's kind colors, and an
/// unfocused pane dims them all.
#[test]
fn mark_colors_dim_with_an_unfocused_pane() {
    let theme = Stoat::test().stoat.theme.clone();
    let rgb =
        |scope: &str| stoat::minimap::color_to_rgb(theme.get(scope).fg.expect("the theme sets it"));
    let colors = stoat::render::editor::DiffMarkColors::resolve(&theme);
    let dimmed = colors.dim([0, 0, 0], 0.5);
    let kinds = [
        DiffHunkStatus::Added,
        DiffHunkStatus::Modified,
        DiffHunkStatus::Moved,
        DiffHunkStatus::Deleted,
    ];
    let scopes = [
        s::DIFF_ADDED,
        s::DIFF_MODIFIED,
        s::DIFF_MOVED,
        s::DIFF_DELETED,
    ];

    assert_eq!(
        [
            kinds.map(|kind| colors.for_status(kind)),
            kinds.map(|kind| dimmed.for_status(kind)),
        ],
        [
            scopes.map(rgb),
            scopes.map(|scope| stoat::render::paint::dim_rgb(rgb(scope), [0, 0, 0], 0.5)),
        ],
    );
}

#[test]
fn relative_line_numbers_fall_back_to_absolute() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/relnum-abs");
    let path = root.join("a.txt");
    h.fake_fs()
        .insert_file(&path, b"one\ntwo\nthree\nfour\nfive");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    dispatch(&mut h.stoat, &MoveDown);
    dispatch(&mut h.stoat, &MoveDown);

    let absolute = ["1", "2", "3", "4", "5"];
    assert_eq!(
        rendered_gutter(&mut h.stoat, true, true, LineNumbers::Relative, 5),
        absolute,
        "insert mode paints absolute"
    );
    assert_eq!(
        rendered_gutter(&mut h.stoat, false, false, LineNumbers::Relative, 5),
        absolute,
        "an unfocused pane paints absolute"
    );
    assert_eq!(
        rendered_gutter(&mut h.stoat, true, false, LineNumbers::Absolute, 5),
        absolute,
        "the Absolute setting paints absolute"
    );
}

/// The diff view's live column counts from the cursor line only for the
/// focused pane outside insert mode, as the plain gutter does.
#[test]
fn the_diff_view_counts_from_the_cursor_only_when_focused_outside_insert() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/relnum-diff");
    let path = root.join("a.txt");
    h.fake_fs()
        .insert_file(&path, b"one\ntwo\nthree\nfour\nfive");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .set_diff_view(true);
    dispatch(&mut h.stoat, &MoveDown);
    dispatch(&mut h.stoat, &MoveDown);

    let absolute = ["1", "2", "3", "4", "5"];
    assert_eq!(
        [
            rendered_diff_numbers(&mut h.stoat, true, false, 5),
            rendered_diff_numbers(&mut h.stoat, true, true, 5),
            rendered_diff_numbers(&mut h.stoat, false, false, 5),
        ],
        [["2", "1", "3", "1", "2"], absolute, absolute],
        "focused in normal mode, focused in insert mode, unfocused"
    );
}

/// A cursor resting on the buffer end is zero-width, and it still draws a
/// block on the cell past the last character.
///
/// Nothing else paints that cell, so a cursor there is either drawn from
/// its own offset or invisible. Rendering without a scene is what puts the
/// primary through the painting path rather than handing its position to
/// the terminal.
#[test]
fn a_cursor_on_the_buffer_end_draws_a_block_there() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/eof-cursor");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"ab\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    for _ in 0..3 {
        dispatch(&mut h.stoat, &MoveRight);
    }

    let theme = stoat::theme::Theme::empty();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    let area = Rect::new(0, 0, 12, 4);
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        fallback,
        &theme,
        &chrome,
        &mut buf,
        true,
        false,
        LineNumbers::Absolute,
        false,
        None,
        None,
        None,
        None,
        true,
        None,
        0.0,
        DiffDials::shipped(),
        WrapMode::None,
        80,
        0,
        &[],
        false,
        None,
        None,
        false,
    );

    // Row 1 is the line the trailing newline opens, and the gutter takes
    // the columns before `gutter_width`.
    let x = editor.gutter_width;
    assert!(
        buf[(x, 1)]
            .style()
            .add_modifier
            .contains(Modifier::REVERSED),
        "the block paints on the buffer-end cell",
    );
}

/// A cursor on the newline of a line as wide as the text area still draws
/// a block.
///
/// The wrap keeps the last column free, so the line breaks one character
/// early and its newline cell lands inside the area, not one column past
/// its right edge.
#[test]
fn a_cursor_after_a_full_width_line_draws_a_block() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/full-width");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"abcdefghijkl\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    for _ in 0..12 {
        dispatch(&mut h.stoat, &MoveRight);
    }

    let theme = stoat::theme::Theme::empty();
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let chrome = stoat::render::editor::ResolvedChrome::resolve(&theme);
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    let area = Rect::new(0, 0, 12, 4);
    let mut buf = Buffer::empty(area);
    stoat::render::editor::render_editor_with_overlay(
        editor,
        area,
        fallback,
        &theme,
        &chrome,
        &mut buf,
        true,
        false,
        LineNumbers::Off,
        false,
        None,
        None,
        None,
        None,
        true,
        None,
        0.0,
        DiffDials::shipped(),
        WrapMode::EditorWidth,
        80,
        0,
        &[],
        false,
        None,
        None,
        false,
    );

    assert!(
        buf[(5, 1)]
            .style()
            .add_modifier
            .contains(Modifier::REVERSED),
        "the block paints on the newline cell after the wrapped l and its indent",
    );
}

#[test]
fn scratch_gutter_numbers_its_one_line() {
    let mut h = Stoat::test();
    // A bare scratch holds an empty rope, which is one line with nothing
    // on it. The row below is past the buffer and carries no number.
    assert_eq!(
        rendered_gutter(&mut h.stoat, true, false, LineNumbers::Relative, 2),
        ["1", ""],
    );
}

#[test]
fn absolute_gutter_numbers_the_final_line() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/phantom-abs");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"one\ntwo\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    assert_eq!(
        rendered_gutter(&mut h.stoat, true, false, LineNumbers::Absolute, 3),
        ["1", "2", "3"],
        "the line the trailing newline opens is numbered like the rest",
    );
}

/// The line a trailing newline opens counts toward the gutter width, since
/// the cursor reaches it and it carries a number of its own.
#[test]
fn a_trailing_newline_widens_the_gutter_at_a_digit_boundary() {
    let width_of = |contents: &[u8]| {
        let mut h = Stoat::test();
        let root = PathBuf::from("/gutter-width");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, contents);
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        rendered_gutter(&mut h.stoat, true, false, LineNumbers::Absolute, 5);
        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .gutter_width
    };
    // 99 lines of text plus the line the trailing newline opens is 100, so
    // the gutter takes a third digit that the same text without the newline
    // does not need.
    let with_newline = "x\n".repeat(99);
    let without_newline = format!("{}x", "x\n".repeat(98));
    assert!(
        width_of(with_newline.as_bytes()) > width_of(without_newline.as_bytes()),
        "the hundredth line needs a digit the ninety-ninth does not"
    );
}

#[test]
fn severity_colors_resolve_under_the_shipped_theme() {
    let h = Stoat::test();
    assert!(
        stoat::render::editor::severity_colors(&h.stoat.theme).is_some(),
        "the shipped default theme must resolve every diagnostic severity \
         to RGB so the sub-cell gutter engages under stoatty",
    );
}

/// The chrome is resolved once and reused across frames, so it has to
/// notice a theme replacement. Not every replacement bumps `theme_epoch`,
/// which is why the cache keys on the theme handle instead.
#[test]
fn the_chrome_resolves_once_and_refreshes_on_a_new_theme() {
    let mut h = Stoat::test();
    h.stoat.refresh_chrome();
    let first = h
        .stoat
        .chrome
        .as_ref()
        .expect("resolved above")
        .1
        .diff_marks;

    h.stoat.refresh_chrome();
    assert_eq!(
        h.stoat.chrome.as_ref().expect("still resolved").0.as_ref() as *const _,
        h.stoat.theme.as_ref() as *const _,
        "an unchanged theme reuses what was already resolved",
    );

    // A theme replacement that leaves theme_epoch alone, which is what
    // reloading the config does.
    let epoch = h.stoat.theme_epoch;
    h.stoat.theme = std::sync::Arc::new(stoat::theme::Theme::empty());
    h.stoat.refresh_chrome();
    assert_eq!(h.stoat.theme_epoch, epoch, "the epoch did not move");

    let after = h.stoat.chrome.as_ref().expect("re-resolved").1.diff_marks;
    assert_ne!(
        (first.added, first.deleted),
        (after.added, after.deleted),
        "the new theme's diff colors replaced the old ones",
    );
}

/// The `(len, color, style)` of each undercurl span that one frame
/// collects for `diagnostic` over `alpha\nbravo\n`, beside the harness
/// that painted it.
fn collected_undercurls(
    diagnostic: Diagnostic,
) -> (
    stoat::test_harness::TestHarness,
    Vec<(u16, [u8; 3], UnderlineKind)>,
) {
    let mut h = Stoat::test();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/undercurl-test");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    h.seed_diagnostics(path, vec![diagnostic]);

    let _ = h.stoat.render();
    let spans = h
        .stoat
        .pending_undercurls
        .spans()
        .iter()
        .map(|span| (span.len, span.color, span.style))
        .collect();
    (h, spans)
}

fn shipped_rgb(h: &stoat::test_harness::TestHarness, scope: &str) -> [u8; 3] {
    stoat::render::paint::style_rgb(h.stoat.theme.get(scope).fg)
        .expect("the shipped theme colors it")
}

#[test]
fn a_diagnostic_span_collects_an_undercurl_under_stoatty() {
    let (h, spans) = collected_undercurls(diag(0, DiagnosticSeverity::WARNING));
    assert_eq!(
        spans,
        [(
            1,
            shipped_rgb(&h, s::UI_DIAGNOSTIC_WARNING),
            UnderlineKind::Curly
        )],
        "the warning paints one wavy run in the shipped warning color",
    );
}

#[test]
fn a_hint_collects_a_dotted_undercurl_in_its_own_color() {
    let (h, spans) = collected_undercurls(diag(0, DiagnosticSeverity::HINT));
    assert_eq!(
        spans,
        [(
            1,
            shipped_rgb(&h, s::UI_DIAGNOSTIC_HINT),
            UnderlineKind::Dotted
        )],
    );
}

/// Unused code reads at the muted text's 45 percent, its dotted line
/// included, and takes the hint color whatever its severity. A tagged
/// warning, such as an unused variable, reads the same.
#[test]
fn unused_code_collects_a_dotted_undercurl_in_the_muted_hint_color() {
    for severity in [DiagnosticSeverity::INFORMATION, DiagnosticSeverity::WARNING] {
        let (h, spans) = collected_undercurls(tagged_overlap_diag(0, 0, 5, severity));
        let theme = &h.stoat.theme;
        let bg = stoat::render::paint::style_rgb(
            theme.get(s::UI_TEXT).bg.or(theme.get(s::UI_BACKGROUND).bg),
        )
        .expect("a themed pane background");
        assert_eq!(
            spans,
            [(
                5,
                stoat::render::editor::mute_rgb(shipped_rgb(&h, s::UI_DIAGNOSTIC_HINT), bg),
                UnderlineKind::Dotted,
            )],
            "{severity:?}",
        );
    }
}

/// The paint bounds which spans it walks by the viewport. A span opening far
/// above a scrolled viewport and closing inside it still paints, and the
/// short ones it was sorted among do not.
#[test]
fn a_span_reaching_into_a_scrolled_viewport_still_paints() {
    let mut h = Stoat::test();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/undercurl-scrolled");
    let path = root.join("a.txt");
    let text = vec!["alpha"; 60].join("\n");
    h.fake_fs().insert_file(&path, text.as_bytes());
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();

    // Short hints on the top rows, all closing well above the viewport, plus
    // one warning opening on row 0 and closing inside it.
    let mut diags: Vec<Diagnostic> = (0..20)
        .map(|line| overlap_diag(line, 0, 1, DiagnosticSeverity::HINT))
        .collect();
    diags.push(Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 45,
                character: 3,
            },
        },
        severity: Some(DiagnosticSeverity::WARNING),
        message: String::new(),
        ..Default::default()
    });
    h.seed_diagnostics(path, diags);

    action_handlers::focused_editor_mut(&mut h.stoat)
        .unwrap()
        .scroll_row = 40;
    let _ = h.stoat.render();

    let colors: Vec<[u8; 3]> = h
        .stoat
        .pending_undercurls
        .spans()
        .iter()
        .map(|u| u.color)
        .collect();
    let warning = stoat::render::paint::style_rgb(h.stoat.theme.get(s::UI_DIAGNOSTIC_WARNING).fg)
        .expect("the shipped warning color");
    assert!(
        !colors.is_empty() && colors.iter().all(|c| *c == warning),
        "only the warning reaches the viewport, got {colors:?}",
    );
}

/// The paint refills collections that outlive it on the editor, so each one
/// has to start from empty. A leaked ordered subset repaints spans the
/// diagnostics have since dropped.
#[test]
fn a_repaint_drops_the_spans_the_diagnostics_no_longer_carry() {
    let mut h = Stoat::test();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    h.stoat.set_apc_tx(tx);

    let root = PathBuf::from("/undercurl-repaint");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();

    h.seed_diagnostics(
        path.clone(),
        vec![
            overlap_diag(0, 0, 5, DiagnosticSeverity::WARNING),
            overlap_diag(1, 0, 5, DiagnosticSeverity::WARNING),
        ],
    );
    let _ = h.stoat.render();
    assert_eq!(
        h.stoat.pending_undercurls.spans().len(),
        2,
        "both spans paint"
    );

    h.seed_diagnostics(
        path,
        vec![overlap_diag(2, 0, 5, DiagnosticSeverity::WARNING)],
    );
    let _ = h.stoat.render();
    assert_eq!(
        h.stoat.pending_undercurls.spans().len(),
        1,
        "only the span the new set carries paints",
    );
}

/// A leaked mute set skips the cells it blended last frame, so the inactive
/// region stops being greyed from the second paint on.
#[test]
fn an_unchanged_repaint_mutes_the_same_cells() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/diag-remute");
    let path = root.join("a.rs");
    h.fake_fs().insert_file(&path, b"let x = 1;\nlet y = 2;\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();

    h.seed_diagnostics(
        path,
        vec![tagged_overlap_diag(1, 0, 10, DiagnosticSeverity::HINT)],
    );

    let first = h.render_composited();
    let second = h.render_composited();
    assert_eq!(first, second, "nothing moved, so the paint repeats exactly");
}

/// A harness over the open file `abc\nxyz\nend\n`, with its second line
/// washed by a reload flash, and the diff view on when `diff_view` says so.
///
/// The cursor rests on the third line, so the cursor line's fill reaches
/// neither of the first two.
fn flashing_file(diff_view: bool) -> stoat::test_harness::TestHarness {
    let mut h = Stoat::test();
    let root = PathBuf::from("/reload-flash");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"abc\nxyz\nend\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
    if diff_view {
        action_handlers::review::enter_diff_view(&mut h.stoat);
        h.settle();
    }
    h.stoat.collapse_focused_cursor_to("abc\nxyz\n".len());

    let buffer = h.stoat.focused_editor_ids().expect("editor").1;
    flash_over(&mut h, buffer, (4, 7));
    h
}

/// Make the active workspace's one reload flash the bytes `start..end` of
/// `buffer`.
fn flash_over(
    h: &mut stoat::test_harness::TestHarness,
    buffer: BufferId,
    (start, end): (usize, usize),
) {
    let ws = h.stoat.active_workspace_mut();
    let spans = {
        let shared = ws.buffers.get(buffer).expect("buffer");
        let guard = shared.read().expect("buffer poisoned");
        vec![(
            guard.snapshot.anchor_at(start, Bias::Right),
            guard.snapshot.anchor_at(end, Bias::Left),
        )]
    };
    ws.reload_flashes = vec![ReloadFlash {
        buffer,
        spans,
        age: 0.0,
    }];
}

/// The backgrounds of the rightmost cells on screen that spell `text`.
fn backgrounds_of(buf: &Buffer, text: &str) -> Vec<Color> {
    let wanted: Vec<String> = text.chars().map(String::from).collect();
    (0..buf.area.height)
        .find_map(|y| {
            let row: Vec<&str> = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            let x = row
                .windows(wanted.len())
                .rposition(|cells| cells.iter().zip(&wanted).all(|(cell, ch)| cell == ch))?;
            Some(
                (x..x + wanted.len())
                    .map(|x| buf[(x as u16, y)].bg)
                    .collect(),
            )
        })
        .expect("the text is on screen")
}

/// A flash washes the cells of its text, the wash fades as the flash ages,
/// and the cells read as unwashed once the fade ends.
///
/// The ages advance with nothing else moving, so each paint is a fresh one
/// rather than a replay of the frame before.
#[test]
fn a_reload_flash_washes_its_cells_and_fades_out() {
    let mut h = flashing_file(false);
    let mut paint = |age: f32| {
        h.stoat.active_workspace_mut().reload_flashes[0].age = age;
        let buf = h.render_composited();
        (backgrounds_of(&buf, "abc"), backgrounds_of(&buf, "xyz"))
    };
    let (plain, washed) = paint(0.0);
    let (_, fading) = paint(0.4);
    let (_, faded) = paint(0.6);

    let distance = |a: Color, b: Color| match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            r1.abs_diff(r2) as u32 + g1.abs_diff(g2) as u32 + b1.abs_diff(b2) as u32
        },
        _ => panic!("the theme paints RGB backgrounds"),
    };
    assert_eq!(
        (
            washed.iter().all(|&bg| bg == washed[0]),
            fading.iter().all(|&bg| bg == fading[0]),
            distance(fading[0], plain[0]) < distance(washed[0], plain[0]),
            fading[0] != plain[0],
            faded,
        ),
        (true, true, true, true, plain),
        "a full wash, a weaker one, then none",
    );
}

/// The diff view washes the right column, where the buffer's own text
/// paints.
#[test]
fn a_reload_flash_washes_the_diff_view_right_column() {
    let mut h = flashing_file(true);
    let buf = h.render_composited();
    let (plain, washed) = (backgrounds_of(&buf, "abc"), backgrounds_of(&buf, "xyz"));

    assert_eq!(
        (
            washed.iter().all(|&bg| bg == washed[0]),
            washed[0] != plain[0]
        ),
        (true, true),
        "the flashed line's cells share one wash that the line above lacks",
    );
}

/// A flash names its buffer, so an editor of another buffer paints none of
/// it.
#[test]
fn a_reload_flash_washes_only_its_own_buffer() {
    let mut h = flashing_file(false);
    h.stoat.active_workspace_mut().reload_flashes[0].buffer = BufferId::new(999);
    let buf = h.render_composited();

    assert_eq!(backgrounds_of(&buf, "xyz"), backgrounds_of(&buf, "abc"));
}

/// A changes view washes the excerpt of a flashing buffer, and the wash
/// fades across paints of a stack that nothing else moves.
///
/// The paint before the flash is the stack's capture. A paint under the
/// flash that replays it shows the line unwashed.
#[test]
fn a_reload_flash_fades_in_a_changes_view_excerpt() {
    let mut h = stoat::test_harness::TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "abc\nkeep\n", "xyz\nkeep\n")]);
    h.cover_with_diff_changes(false);
    h.settle();
    let buffer = h
        .stoat
        .active_workspace()
        .buffers
        .id_for_path(std::path::Path::new("/repo/a.txt"))
        .expect("the excerpt's buffer");
    let tinted = backgrounds_of(&h.render_composited(), "xyz");
    flash_over(&mut h, buffer, (0, 3));

    let mut paint = |age: f32| {
        h.stoat.active_workspace_mut().reload_flashes[0].age = age;
        backgrounds_of(&h.render_composited(), "xyz")
    };
    let (washed, fading, faded) = (paint(0.0), paint(0.4), paint(0.6));

    assert_eq!(
        (washed != tinted, fading != washed, fading != tinted, faded),
        (true, true, true, tinted),
        "a full wash, a weaker one, then none",
    );
}

#[test]
fn snapshot_diagnostic_gutter_renders_severity_glyphs() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/diag-test");
    let path = root.join("a.txt");
    h.fake_fs()
        .insert_file(&path, b"alpha\nbravo\ncharlie\ndelta\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    h.seed_diagnostics(
        path,
        vec![
            diag(0, DiagnosticSeverity::ERROR),
            diag(1, DiagnosticSeverity::WARNING),
            diag(2, DiagnosticSeverity::INFORMATION),
            diag(3, DiagnosticSeverity::HINT),
        ],
    );
    h.assert_snapshot("diagnostic_gutter_each_severity");
}

#[test]
fn snapshot_diagnostic_gutter_worst_severity_per_row() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/diag-worst");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    h.seed_diagnostics(
        path,
        vec![
            diag(0, DiagnosticSeverity::WARNING),
            diag(0, DiagnosticSeverity::ERROR),
        ],
    );
    h.assert_snapshot("diagnostic_gutter_worst_severity_wins");
}

#[test]
fn snapshot_diagnostic_inline_underline_span() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/diag-inline");
    let path = root.join("a.rs");
    h.fake_fs()
        .insert_file(&path, b"let x = 1;\nlet y = 2;\nlet z = 3;\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    // The diagnostic sits on line 1 while the cursor stays on line 0. The
    // span is underlined, and its message trails line 1 as it trails every
    // line a diagnostic starts on.
    h.seed_diagnostics(
        path,
        vec![Diagnostic {
            range: Range {
                start: Position {
                    line: 1,
                    character: 4,
                },
                end: Position {
                    line: 1,
                    character: 5,
                },
            },
            severity: Some(DiagnosticSeverity::WARNING),
            message: "unused variable".into(),
            ..Default::default()
        }],
    );
    h.assert_snapshot("diagnostic_inline_underline_span");
}

fn overlap_diag(line: u32, start: u32, end: u32, sev: DiagnosticSeverity) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line,
                character: start,
            },
            end: Position {
                line,
                character: end,
            },
        },
        severity: Some(sev),
        message: String::new(),
        ..Default::default()
    }
}

fn tagged_overlap_diag(line: u32, start: u32, end: u32, sev: DiagnosticSeverity) -> Diagnostic {
    Diagnostic {
        tags: Some(vec![DiagnosticTag::UNNECESSARY]),
        ..overlap_diag(line, start, end, sev)
    }
}

#[test]
fn snapshot_diagnostic_overlap_warning_beats_hint() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/diag-overlap-warn");
    let path = root.join("a.rs");
    h.fake_fs()
        .insert_file(&path, b"let x = 1;\nlet y = 2;\nlet z = 3;\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    // Same span, warning then hint in rust-analyzer's publish order. The
    // worse severity must win the span color over the later-published hint,
    // so the underline stays warning yellow rather than turning hint grey.
    h.seed_diagnostics(
        path,
        vec![
            overlap_diag(1, 4, 5, DiagnosticSeverity::WARNING),
            overlap_diag(1, 4, 5, DiagnosticSeverity::HINT),
        ],
    );
    h.assert_snapshot("diagnostic_overlap_warning_beats_hint");
}

#[test]
fn snapshot_diagnostic_overlap_error_beats_hint() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/diag-overlap-error");
    let path = root.join("a.rs");
    h.fake_fs()
        .insert_file(&path, b"let x = 1;\nlet y = 2;\nlet z = 3;\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    // Hint then error in publish order. Error must win the span color.
    h.seed_diagnostics(
        path,
        vec![
            overlap_diag(1, 4, 5, DiagnosticSeverity::HINT),
            overlap_diag(1, 4, 5, DiagnosticSeverity::ERROR),
        ],
    );
    h.assert_snapshot("diagnostic_overlap_error_beats_hint");
}

#[test]
fn snapshot_diagnostic_unnecessary_mutes_syntax() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/diag-unnecessary");
    let path = root.join("a.rs");
    h.fake_fs()
        .insert_file(&path, b"let x = 1;\nlet y = 2;\nlet z = 3;\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    // An Unnecessary-tagged hint marks inactive code. Its span blends each
    // token's syntax fg toward the background, keeping the per-token hues
    // rather than flattening the line to one hint color. Its dotted line
    // rides the undercurl overlay, so no cell carries an underline.
    h.seed_diagnostics(
        path,
        vec![tagged_overlap_diag(1, 0, 10, DiagnosticSeverity::HINT)],
    );
    h.assert_snapshot("diagnostic_unnecessary_mutes_syntax");
}

#[test]
fn snapshot_diagnostic_warning_over_unnecessary_still_underlines() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/diag-warn-over-unnecessary");
    let path = root.join("a.rs");
    h.fake_fs()
        .insert_file(&path, b"let x = 1;\nlet y = 2;\nlet z = 3;\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    // A warning overlapping an inactive-code region sorts last and paints on
    // top, so its underline lands over the muted span rather than being
    // erased by the mute.
    h.seed_diagnostics(
        path,
        vec![
            tagged_overlap_diag(1, 0, 10, DiagnosticSeverity::HINT),
            overlap_diag(1, 4, 5, DiagnosticSeverity::WARNING),
        ],
    );
    h.assert_snapshot("diagnostic_warning_over_unnecessary_still_underlines");
}

/// A tagged warning, which is how rustc sends an unused variable, reads as
/// unused code. Its span mutes and takes no underline in the cells. The
/// gutter mark and the message after the line keep the warning color.
#[test]
fn snapshot_diagnostic_unused_warning_reads_as_unused() {
    let mut h = Stoat::test();
    let path = open_settled(
        &mut h,
        "/diag-unused-warning",
        b"let x = 1;\nlet y = 2;\nlet z = 3;\n",
    );
    h.seed_diagnostics(
        path,
        vec![Diagnostic {
            tags: Some(vec![DiagnosticTag::UNNECESSARY]),
            ..message_diag((1, 4), (1, 5), DiagnosticSeverity::WARNING, "unused `y`")
        }],
    );
    h.assert_snapshot("diagnostic_unused_warning_reads_as_unused");
}

#[test]
fn unnecessary_span_blends_a_shared_cell_once() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/diag-unnecessary-dedup");
    let path = root.join("a.rs");
    h.fake_fs()
        .insert_file(&path, b"let x = 1;\nlet y = 2;\nlet z = 3;\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();

    let row_fg = |stoat: &mut Stoat| {
        let buf = stoat.render();
        (0..buf.area.width)
            .map(|x| buf[(x, 1)].fg)
            .collect::<Vec<_>>()
    };

    h.seed_diagnostics(
        path.clone(),
        vec![tagged_overlap_diag(1, 0, 10, DiagnosticSeverity::HINT)],
    );
    let once = row_fg(&mut h.stoat);

    h.seed_diagnostics(
        path,
        vec![
            tagged_overlap_diag(1, 0, 10, DiagnosticSeverity::HINT),
            tagged_overlap_diag(1, 0, 10, DiagnosticSeverity::HINT),
        ],
    );
    let twice = row_fg(&mut h.stoat);

    assert_eq!(
        once, twice,
        "overlapping muted spans must blend a shared cell exactly once"
    );
}

#[test]
fn snapshot_diagnostic_cursor_line_eol_message() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/diag-eol");
    let path = root.join("a.rs");
    h.fake_fs().insert_file(&path, b"let x = 1;\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    // The cursor opens on line 0. The diagnostic underlines its span, and
    // its message trails the line content, dimmed in the severity color.
    h.seed_diagnostics(
        path,
        vec![Diagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 4,
                },
                end: Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            message: "mismatched types".into(),
            ..Default::default()
        }],
    );
    h.assert_snapshot("diagnostic_cursor_line_eol_message");
}

/// Every line a diagnostic starts on shows its message past the text. The
/// info span starts on line 1 under its warning and runs to line 3, and it
/// prints on neither line 2 nor line 3, so the hint on line 2 keeps it. A
/// message that runs into the pane's edge ends in an ellipsis.
#[test]
fn snapshot_diagnostic_eol_message_on_every_line() {
    let mut h = Stoat::test();
    let path = open_settled(
        &mut h,
        "/diag-eol-every-line",
        b"let x = 1;\nlet y = 2;\nlet z = 3;\n",
    );
    h.seed_diagnostics(
        path,
        vec![
            message_diag(
                (0, 4),
                (0, 5),
                DiagnosticSeverity::ERROR,
                "mismatched types",
            ),
            message_diag((1, 4), (1, 5), DiagnosticSeverity::WARNING, "unused `y`"),
            message_diag((1, 0), (3, 0), DiagnosticSeverity::INFORMATION, "two lines"),
            message_diag(
                (2, 8),
                (2, 9),
                DiagnosticSeverity::HINT,
                "consider naming this literal as a constant so that every reader agrees on it",
            ),
        ],
    );
    h.assert_snapshot("diagnostic_eol_message_on_every_line");
}

/// The cursor's line reads out a span the cursor sits inside, though the
/// span starts above it. The line the span starts on keeps its message, and
/// the line it ends on prints nothing.
#[test]
fn the_cursor_line_reads_a_span_reaching_it_from_above() {
    let mut h = Stoat::test();
    h.stoat.stoatty = false;
    let path = open_settled(
        &mut h,
        "/diag-eol-straddle",
        b"fn main() {\n    let x = 1;\n}\n",
    );
    h.seed_diagnostics(
        path,
        vec![message_diag(
            (0, 0),
            (2, 1),
            DiagnosticSeverity::WARNING,
            "never called",
        )],
    );
    dispatch(&mut h.stoat, &MoveDown);

    let buf = h.stoat.render();
    assert_eq!(
        [(0, "fn main() {"), (1, "let x = 1;"), (2, "}")]
            .map(|(y, text)| tail_after(&buf, y, text)),
        [Some("never called"), Some("never called"), Some("")].map(|tail| tail.map(String::from)),
    );
}

/// The popover shows the diagnostic under the cursor, so its message prints
/// on no line, while another line keeps its own. The other line sits below
/// the popover box, whose cells cover the rows under the cursor.
#[test]
fn the_popover_diagnostic_prints_on_no_line() {
    let mut h = Stoat::test();
    let path = open_settled(
        &mut h,
        "/diag-eol-popover",
        b"let x = 1;\n\n\n\n\n\nlet y = 2;\n",
    );
    h.seed_diagnostics(
        path,
        vec![
            message_diag((0, 0), (0, 3), DiagnosticSeverity::ERROR, "bad"),
            message_diag((6, 4), (6, 5), DiagnosticSeverity::WARNING, "unused `y`"),
        ],
    );

    let buf = h.stoat.render();
    assert_eq!(
        [(0, "let x = 1;"), (6, "2;")].map(|(y, text)| tail_after(&buf, y, text)),
        [Some(""), Some("unused `y`")].map(|tail| tail.map(String::from)),
    );
}

/// A soft-wrapped line prints its message after its last row, where the
/// line ends, rather than after the row the wrap cut.
#[test]
fn a_wrapped_line_prints_its_message_after_its_last_row() {
    let mut h = Stoat::test();
    h.stoat.settings.editor_wrap = Some(WrapMode::Bounded);
    h.stoat.settings.editor_wrap_column = Some(20);
    let path = open_settled(
        &mut h,
        "/diag-eol-wrap",
        b"let first = alpha + beta + gamma;\n",
    );
    h.seed_diagnostics(
        path,
        vec![message_diag(
            (0, 4),
            (0, 9),
            DiagnosticSeverity::WARNING,
            "never read",
        )],
    );

    let buf = h.stoat.render();
    let row = |y| tail_after(&buf, y, "").unwrap_or_default();
    assert_eq!(
        (
            row(0).contains("never read"),
            row(1).ends_with("gamma;   never read")
        ),
        (false, true),
        "rows: {:?}",
        (row(0), row(1)),
    );
}

/// Open `text` as `a.rs` under `root` and settle, returning its path.
fn open_settled(h: &mut stoat::test_harness::TestHarness, root: &str, text: &[u8]) -> PathBuf {
    let root = PathBuf::from(root);
    let path = root.join("a.rs");
    h.fake_fs().insert_file(&path, text);
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    path
}

/// A diagnostic over `start..end`, each a `(line, character)` pair, that
/// reads `message`.
fn message_diag(
    start: (u32, u32),
    end: (u32, u32),
    severity: DiagnosticSeverity,
    message: &str,
) -> Diagnostic {
    let at = |(line, character)| Position { line, character };
    Diagnostic {
        range: Range {
            start: at(start),
            end: at(end),
        },
        severity: Some(severity),
        message: message.into(),
        ..Default::default()
    }
}

/// What row `y` of `buf` shows past the first `text` on it, trimmed, or
/// `None` when the row does not hold `text`.
fn tail_after(buf: &Buffer, y: u16, text: &str) -> Option<String> {
    let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
    row.split_once(text)
        .map(|(_, tail)| tail.trim().to_string())
}

fn add_cursor_at(stoat: &mut Stoat, offset: usize) {
    let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let anchor = buffer_snapshot.anchor_at(offset, Bias::Left);
    editor
        .selections
        .insert_cursor(anchor, SelectionGoal::None, buffer_snapshot);
}

#[test]
fn snapshot_stoatty_delegates_only_primary_cursor() {
    let mut h = Stoat::test();
    h.stoat.settings.editor_minimap = Some(stoat_config::MinimapMode::PerPane);
    let root = PathBuf::from("/cursor-stoatty");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha bravo charlie\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    add_cursor_at(&mut h.stoat, 6);

    h.assert_snapshot("stoatty_primary_cursor_delegated");
}

#[test]
fn primary_cursor_screen_pos_matches_painted_cell() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/cursor-pos");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha bravo\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    h.snapshot();
    // Column 6 is the line-number gutter width the cursor sits past.
    assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((6, 0)));

    for _ in 0..6 {
        dispatch(&mut h.stoat, &MoveRight);
    }
    h.snapshot();
    assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((12, 0)));
}

#[test]
fn primary_cursor_screen_pos_none_when_finder_open() {
    let mut h = Stoat::test();
    let root = PathBuf::from("/cursor-finder");
    let path = root.join("a.txt");
    h.fake_fs().insert_file(&path, b"alpha\n");
    h.stoat.active_workspace_mut().git_root = root;
    dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();

    h.snapshot();
    // Column 6 is the line-number gutter width the cursor sits past.
    assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((6, 0)));

    dispatch(&mut h.stoat, &OpenFileFinder);
    h.settle();
    h.snapshot();
    assert_eq!(h.stoat.primary_cursor_screen_pos(), None);
}

#[test]
fn snapshot_selection_over_tab_line() {
    let mut h = stoat::test_harness::TestHarness::with_size(20, 4);
    let path = h.write_file("s.txt", "ab\tcd\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &ExtendToLineEnd);
    h.assert_snapshot("selection_over_tab_line");
}

/// A soft-wrapped row leaves the accumulating paint path, so the general
/// one resolves each character from a buffer point it carries forward. That
/// point's column counts bytes, so an accent moves it two and a drift of one
/// per character walks the wash off the text it covers.
#[test]
fn snapshot_selection_over_a_wrapped_line_of_accents() {
    let mut h = stoat::test_harness::TestHarness::with_size(20, 4);
    let path = h.write_file("s.txt", &format!("{}\n", "\u{e9}".repeat(30)));
    h.open_file(&path);
    dispatch(&mut h.stoat, &ExtendToLineEnd);
    h.assert_snapshot("selection_over_wrapped_accents");
}

/// Tabs and wide glyphs on a wrapped row are where an accumulated column is
/// likeliest to part ways with a resolved one. A tab's width depends on
/// where it starts, and a wide glyph moves the column by two.
///
/// The paint loop's debug assertion turns this into a per-character
/// comparison against the authoritative mapping. The snapshot pins the
/// cells that come out of it.
#[test]
fn snapshot_selection_over_a_wrapped_line_of_tabs_and_wide_chars() {
    let mut h = stoat::test_harness::TestHarness::with_size(20, 5);
    let path = h.write_file(
        "s.txt",
        "\u{4e16}\u{754c}\ta\u{4e16}b\t\u{754c}cd\u{4e16}\te\u{754c}f\n",
    );
    h.open_file(&path);
    dispatch(&mut h.stoat, &ExtendToLineEnd);
    h.assert_snapshot("selection_over_wrapped_tabs_and_wide");
}

#[test]
fn snapshot_selection_over_wide_chars() {
    let mut h = stoat::test_harness::TestHarness::with_size(20, 4);
    let path = h.write_file("s.txt", "a世z\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &ExtendToLineEnd);
    // The text pass advances by display width per glyph, so glyphs after a
    // wide char stay aligned with the selection and cursor columns, which
    // also account for display width. This locks that width-aware column math.
    h.assert_snapshot("selection_over_wide_chars");
}

#[test]
fn snapshot_selection_spanning_fold() {
    let mut h = stoat::test_harness::TestHarness::with_size(20, 4);
    let path = h.write_file("s.txt", "abcdefgh\nij\n");
    h.open_file(&path);
    h.settle();
    {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        editor
            .display_map
            .fold(vec![Point::new(0, 2)..Point::new(0, 6)]);
    }
    dispatch(&mut h.stoat, &ExtendToLineEnd);
    h.assert_snapshot("selection_spanning_fold");
}

/// The Blame Column moves a blamed pane's text 18 cells right, and the
/// gutter width counts it.
#[test]
fn the_blame_column_moves_the_text_right() {
    let mut h = blame_column_harness(120);
    let (_, gutter, text_x) = text_start(&mut h);
    blame_on(&mut h);
    let (_, blamed_gutter, blamed_text_x) = text_start(&mut h);
    assert_eq!((blamed_gutter, blamed_text_x), (gutter + 18, text_x + 18));
}

/// The cursor's line selects the file's one commit, so each line of its
/// run takes ermine's selected bar and the selected surface beside it.
/// The empty row past the last line takes the column's surface alone, and
/// the gutter's own bars move 18 cells right.
#[test]
fn the_blame_column_draws_its_bars_left_of_the_gutter() {
    let mut h = blame_column_harness(120);
    let gutter = lane_bars(&h);
    assert!(!gutter.is_empty(), "the gutter draws its separator");
    blame_on(&mut h);
    let (area, _, _) = text_start(&mut h);
    let column_end = ((area.x + 18) * 16) as i16;
    let (mut column, rest): (Vec<_>, Vec<_>) = lane_bars(&h)
        .into_iter()
        .partition(|&(x, ..)| x < column_end);
    column.sort_by_key(|&(x, y, ..)| (y, x));
    let moved: Vec<_> = gutter
        .into_iter()
        .map(|(x, y, width, height, color)| (x + 18 * 16, y, width, height, color))
        .collect();

    let (left, top) = ((area.x * 16) as i16, (area.y * 16) as i16);
    let (bar, selected, surface) = (SELECTED_BAR, SELECTED_SURFACE, [0x14, 0x16, 0x1a]);
    assert_eq!(
        (column, rest),
        (
            vec![
                (left, top, 4, 16, bar),
                (left + 4, top, 277, 16, selected),
                (left, top + 16, 4, 16, bar),
                (left + 4, top + 16, 277, 16, selected),
                (left, top + 32, 281, 16, surface),
            ],
            moved
        ),
        "the run's bars and the surface, and the gutter's bars 18 cells right",
    );
}

/// The first line of a blamed run reads its commit's short sha at the
/// label's start, past the run's bar and its gap.
#[test]
fn a_blamed_run_names_its_short_sha_on_its_first_line() {
    let mut h = blame_column_harness(120);
    blame_on(&mut h);
    let (area, _, _) = text_start(&mut h);
    let shas: Vec<_> = lane_texts(&h)
        .into_iter()
        .filter(|(text, ..)| text == "c1")
        .collect();
    assert_eq!(
        shas,
        [(
            "c1".to_owned(),
            (area.x * 16 + 14) as i16,
            (area.y * 16) as i16
        )]
    );
}

/// A press on a text cell of a blamed pane lands on the offset that the
/// cell 18 columns to its left gave before the column turned on.
#[test]
fn a_press_past_the_blame_column_lands_on_the_text_under_it() {
    let mut h = blame_column_harness(120);
    let (area, _, text_x) = text_start(&mut h);
    let (editor_id, _) = h.stoat.focused_editor_ids().expect("focused editor");
    let col = text_x - area.x + 3;
    let before = stoat::mouse::editor_screen_to_offset(&mut h.stoat, editor_id, area, col, 1);
    blame_on(&mut h);
    let after = stoat::mouse::editor_screen_to_offset(&mut h.stoat, editor_id, area, col + 18, 1);
    assert_eq!([before, after], [Some(17), Some(17)], "the b of beta");
}

/// A blamed pane wraps its lines at the width its text keeps past the
/// column and the gutter, less the cell the cursor takes past a line's end.
#[test]
fn a_blamed_pane_wraps_at_its_text_width() {
    let mut h = blame_column_harness(120);
    blame_on(&mut h);
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    let text = editor.text_rect.expect("the text painted");
    assert_eq!(
        editor.display_map.wrap_width(),
        Some(u32::from(text.width) - 1)
    );
}

/// With line numbers off, a blamed pane's severity column moves 18 cells
/// right, past the column. There the diagnostic under the cursor opens no
/// popover, since the Blame Card claims the row under the line, and the
/// popover of the diagnostic under the mouse opens 18 cells right of where
/// the cursor's opened.
#[test]
fn the_severity_column_and_its_popover_move_past_the_blame_column() {
    let mut h = blame_column_harness(120);
    h.stoat.settings.editor_line_numbers = Some(LineNumbers::Off);
    h.seed_diagnostics(
        "/repo/a.rs",
        vec![Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 2)),
            severity: Some(DiagnosticSeverity::ERROR),
            message: "expected an item".to_owned(),
            ..Default::default()
        }],
    );
    h.stoat.render();
    let (marks, popovers) = (lane_bars(&h), popover_lefts(&h));
    assert_eq!(
        (marks.len(), popovers.len()),
        (1, 1),
        "one mark and one popover"
    );

    blame_on(&mut h);
    let (area, _, _) = text_start(&mut h);
    let column_end = ((area.x + 18) * 16) as i16;
    let rest: Vec<_> = lane_bars(&h)
        .into_iter()
        .filter(|&(x, ..)| x >= column_end)
        .collect();
    let moved: Vec<_> = marks
        .into_iter()
        .map(|(x, y, width, height, color)| (x + 18 * 16, y, width, height, color))
        .collect();
    let cursor_popovers = popover_lefts(&h);

    let text = {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.text_rect.expect("the text painted")
    };
    h.stoat.hover_cell = Some((text.x, text.y));
    h.stoat.render();
    assert_eq!(
        (rest, cursor_popovers, popover_lefts(&h)),
        (moved, Vec::new(), vec![popovers[0] + 18]),
        "the mark 18 cells right, no popover from the cursor, and the mouse's 18 cells right",
    );
}

/// A pane with no room for the column, its gutter, and 20 text cells keeps
/// its text where it was while it blames.
#[test]
fn a_narrow_pane_shows_no_blame_column() {
    let mut h = blame_column_harness(40);
    let before = text_start(&mut h);
    blame_on(&mut h);
    let blames = {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).blame
    };
    assert_eq!((text_start(&mut h), blames), (before, true));
}

/// The cursor's line selects its commit. Each line of that commit takes
/// ermine's selected bar and surface and no other line does, and a move
/// onto a line of the other commit moves the selection to that commit.
#[test]
fn the_cursor_lines_commit_draws_as_the_selection() {
    let mut h = alternating_blame_harness();
    let first = selection_bars(&column_bars(&h, focused_pane_index(&h)));
    h.type_keys("j");
    h.stoat.render();
    let second = selection_bars(&column_bars(&h, focused_pane_index(&h)));

    let (bar, surface) = (SELECTED_BAR, SELECTED_SURFACE);
    assert_eq!(
        (first, second),
        (
            vec![
                (0, 0, 4, 16, bar),
                (4, 0, 277, 16, surface),
                (0, 32, 4, 16, bar),
                (4, 33, 277, 15, surface),
            ],
            vec![
                (0, 16, 4, 16, bar),
                (4, 17, 277, 15, surface),
                (0, 48, 4, 16, bar),
                (4, 49, 277, 15, surface),
            ],
        ),
        "the first commit's lines, then the second commit's",
    );
}

/// A pane that loses the focus selects no commit. Its column keeps every
/// bar it drew while focused, and none of them takes a selected color.
#[test]
fn an_unfocused_pane_selects_no_commit() {
    let mut h = alternating_blame_harness();
    h.stoat.pane_fade_override = Some(false);
    let index = focused_pane_index(&h);
    let focused = column_bars(&h, index);
    h.type_action("SplitRight()");
    h.settle();
    h.stoat.render();
    let unfocused = column_bars(&h, index);

    let places = |bars: &[BarDraw]| -> Vec<(i16, i16, u16, u16)> {
        bars.iter()
            .map(|&(x, y, width, height, _)| (x, y, width, height))
            .collect()
    };
    assert_eq!(
        (
            selection_bars(&focused).len(),
            selection_bars(&unfocused),
            places(&unfocused),
        ),
        (4, Vec::new(), places(&focused)),
        "the focused paint's selection, then the unfocused paint's bars",
    );
}

/// A cursor on an uncommitted line selects no commit, so no bar takes a
/// selected color. On a committed line the same pane selects the commit
/// of that line.
#[test]
fn a_cursor_on_an_uncommitted_line_selects_no_commit() {
    let mut h = history_harness(120, &[("c1", "one", &[("a.rs", "a\nb\n")])], "a\nb\nc\n");
    blame_on(&mut h);
    let committed = selection_bars(&column_bars(&h, focused_pane_index(&h)));
    h.type_keys("j j");
    h.stoat.render();
    let uncommitted = selection_bars(&column_bars(&h, focused_pane_index(&h)));

    let (bar, surface) = (SELECTED_BAR, SELECTED_SURFACE);
    assert_eq!(
        (committed, uncommitted),
        (
            vec![
                (0, 0, 4, 16, bar),
                (4, 0, 277, 16, surface),
                (0, 16, 4, 16, bar),
                (4, 16, 277, 16, surface),
            ],
            Vec::new(),
        ),
        "the committed lines from line 1, then nothing from line 3",
    );
}

/// The bar and the short sha color of ermine's selected commit.
const SELECTED_BAR: [u8; 3] = [0x7f, 0xb0, 0xff];

/// The surface beside the lines of ermine's selected commit.
const SELECTED_SURFACE: [u8; 3] = [0x1b, 0x22, 0x30];

/// A decoded bar as its place, its size, and its color.
type BarDraw = (i16, i16, u16, u16, [u8; 3]);

/// A 24-row window `width` cells wide over `a.rs` of a repository at
/// `/repo`, painted with its Blame Column off.
///
/// The minimap stays off, so the pane's lane holds the column and the
/// gutter alone.
fn blame_column_harness(width: u16) -> stoat::test_harness::TestHarness {
    let text = "fn alpha() {}\nfn beta() {}\n";
    history_harness(width, &[("c1", "one", &[("a.rs", text)])], text)
}

/// A 120-cell window over `a.rs`, whose lines alternate between two
/// commits, painted with its Blame Column on and its cursor on line 1.
///
/// Each line is a run of its own, so each line past the first draws a
/// divider over its top.
fn alternating_blame_harness() -> stoat::test_harness::TestHarness {
    let text = "a\nB\nc\nD\n";
    let mut h = history_harness(
        120,
        &[
            ("c1", "one", &[("a.rs", "a\nb\nc\nd\n")]),
            ("c2", "two", &[("a.rs", text)]),
        ],
        text,
    );
    blame_on(&mut h);
    h
}

/// A 24-row window `width` cells wide over `a.rs` of a repository at
/// `/repo` that holds `history`, with `text` in the working tree, painted
/// with its Blame Column off.
fn history_harness(
    width: u16,
    history: &[stoat::test_harness::CommitSpec<'_>],
    text: &str,
) -> stoat::test_harness::TestHarness {
    let mut h = stoat::test_harness::TestHarness::with_size(width, 24);
    h.stoat.minimap_override = Some(false);
    h.seed_linear_history("/repo", history);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_fs().insert_file("/repo/a.rs", text);
    h.open_file(std::path::Path::new("/repo/a.rs"));
    h.settle();
    h.stoat.render();
    h
}

/// Turn the focused pane's Blame Column on, and paint.
fn blame_on(h: &mut stoat::test_harness::TestHarness) {
    h.type_keys("space h a");
    h.settle();
    h.stoat.render();
}

/// The focused pane's area, its editor's gutter width, and the column its
/// text starts at.
fn text_start(h: &mut stoat::test_harness::TestHarness) -> (Rect, u16, u16) {
    let area = {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).area
    };
    let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    let text = editor.text_rect.expect("the text painted");
    (area, editor.gutter_width, text.x)
}

/// The bars in the focused pane's lane.
fn lane_bars(h: &stoat::test_harness::TestHarness) -> Vec<BarDraw> {
    pane_lane_bars(h, focused_pane_index(h))
}

/// The bars in the lane of the pane at `index`.
fn pane_lane_bars(h: &stoat::test_harness::TestHarness, index: u32) -> Vec<BarDraw> {
    pane_lane_commands(h, index)
        .into_iter()
        .flat_map(|command| match command {
            Command::Bar(bar) => vec![(bar.x, bar.y, bar.width, bar.height, bar.color)],
            Command::Bars(group) => group
                .bars
                .into_iter()
                .map(|(x, y, width, height)| (x, y, width, height, group.color))
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

/// The text runs in the focused pane's lane, each as its text and its
/// place in sixteenths.
fn lane_texts(h: &stoat::test_harness::TestHarness) -> Vec<(String, i16, i16)> {
    lane_commands(h)
        .into_iter()
        .flat_map(|command| match command {
            Command::TextRun(run) => vec![(run.text, run.col, run.row)],
            Command::TextRuns(group) => group
                .runs
                .into_iter()
                .map(|(col, row, text)| (text, col, row))
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

/// The left cell of each popover in the focused pane's lane.
fn popover_lefts(h: &stoat::test_harness::TestHarness) -> Vec<u16> {
    lane_commands(h)
        .into_iter()
        .filter_map(|command| match command {
            Command::Popover(popover) => Some(popover.left),
            _ => None,
        })
        .collect()
}

/// The commands in the focused pane's lane, as the last paint left them.
fn lane_commands(h: &stoat::test_harness::TestHarness) -> Vec<Command> {
    pane_lane_commands(h, focused_pane_index(h))
}

/// The commands in the lane of the pane at `index`, as the last paint left
/// them.
fn pane_lane_commands(h: &stoat::test_harness::TestHarness, index: u32) -> Vec<Command> {
    let lane = h
        .stoat
        .apc_scene
        .lane_bytes(stoat::render::content_lane(index));
    decode_stream(lane)
}

/// The lane index of the focused pane.
fn focused_pane_index(h: &stoat::test_harness::TestHarness) -> u32 {
    let ws = h.stoat.active_workspace();
    ws.panes.pane(ws.panes.focus()).index
}

/// The Blame Column's bars in the lane of the pane at `index`, each placed
/// from the pane's top left, in place order.
fn column_bars(h: &stoat::test_harness::TestHarness, index: u32) -> Vec<BarDraw> {
    let area = {
        let ws = h.stoat.active_workspace();
        ws.panes
            .split_panes()
            .find(|(_, pane)| pane.index == index)
            .map(|(_, pane)| pane.area)
            .expect("the pane at the index")
    };
    let (left, top) = ((area.x * 16) as i16, (area.y * 16) as i16);
    let mut bars: Vec<BarDraw> = pane_lane_bars(h, index)
        .into_iter()
        .filter(|&(x, ..)| x < left + 18 * 16)
        .map(|(x, y, width, height, color)| (x - left, y - top, width, height, color))
        .collect();
    bars.sort_by_key(|&(x, y, ..)| (y, x));
    bars
}

/// The bars of `bars` in a selected color.
fn selection_bars(bars: &[BarDraw]) -> Vec<BarDraw> {
    bars.iter()
        .copied()
        .filter(|bar| [SELECTED_BAR, SELECTED_SURFACE].contains(&bar.4))
        .collect()
}
