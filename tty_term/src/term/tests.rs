use super::{
    insert_room, journal_past_bounds, mark_selection_change, uncovered_rows, whole_row, Arc,
    Cursor, CursorShape, Damage, MinimapJournal, SelectionRange, TermEvent, Terminal, ESC,
    MAX_CAPTURE_BYTES, MAX_DECORATIONS, MAX_MINIMAP_JOURNAL, MAX_MINIMAP_JOURNAL_LINES,
    MAX_MINIMAP_LINES, MAX_MINIMAP_STORES, MAX_MINIMAP_VIEWS, MAX_POOLS, PAGE_POOL_CAPACITY,
    XTVERSION_REPLY,
};
use crate::{
    grid::{
        Bar, Border, BorderStyle, Cell, DocumentOffset, Flags, Grid, Icon, IconKind, Minimap,
        MinimapView, Overlay, Panel, PanelShadow, Polyline, Rgb, Scale, ScrollRegion, Sketch,
        TextRun, UnderlineStyle,
    },
    theme::Theme,
};
use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line, Point},
};
use stoatty_protocol::command::{
    encode_bar, encode_border, encode_config_reload, encode_fill, encode_fill_decorations_scope,
    encode_fill_end, encode_font_step, encode_hello, encode_icon, encode_ident_reply,
    encode_line_layout, encode_minimap, encode_minimap_drop, encode_minimap_lines,
    encode_minimap_view, encode_panel, encode_polyline, encode_pool_anchor, encode_pool_cursor,
    encode_pool_cursor_release, encode_pool_drop, encode_pool_region, encode_popover,
    encode_reposition, encode_reset, encode_scale, encode_scroll, encode_scroll_region,
    encode_sketch, encode_text_run, encode_window_open, encode_zoom_capture, BarCommand,
    BorderCommand, BorderStyle as ProtoBorderStyle, FillCommand, HelloCommand, IconCommand,
    IconKind as ProtoIconKind, IdentReply, LineLayoutCommand, LineSummary, MinimapCommand,
    MinimapDropCommand, MinimapLinesCommand, MinimapRun, MinimapViewCommand, PanelCommand,
    PanelShadow as ProtoPanelShadow, PolylineCommand, PoolAnchorCommand, PoolCursorCommand,
    PoolCursorReleaseCommand, PoolDropCommand, PoolKind, PoolRegionCommand, PopoverCommand,
    RepositionCommand, ScaleCommand, ScrollCommand, ScrollRegionCommand, SketchBounds,
    SketchCommand, SketchEasing, SketchPhase, SketchShape, SketchStyle, SketchTiming,
    TextRunCommand, WindowOpenCommand,
};

/// Base64, for a test feeding a graphics payload.
fn base64_of(bytes: &[u8]) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .encode(bytes)
        .into_bytes()
}

fn project(rows: usize, cols: usize, bytes: &[u8]) -> (Grid, Cursor) {
    let mut terminal = Terminal::new(rows, cols, Theme::default());
    let mut grid = Grid::new(rows, cols);

    terminal.advance(bytes);
    let (cursor, _scroll, _damage) = terminal.project(&mut grid);

    (grid, cursor)
}

/// The scrolled-back frame renders a composed history window, so the live
/// grid is projected into for nothing. Skipping that has to leave the grid
/// untouched and still report that output arrived.
#[test]
fn taking_the_damage_flag_reports_output_and_leaves_the_grid_alone() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    terminal.advance(b"ab");
    terminal.project(&mut grid);
    assert!(
        !terminal.take_damage_flag(),
        "the projection consumed what had arrived"
    );

    terminal.advance(b"cd");
    assert!(terminal.take_damage_flag(), "the new output is reported");
    assert_eq!(
        (grid.get(0, 0).ch, grid.get(0, 1).ch),
        ('a', 'b'),
        "the grid still holds what the projection put there"
    );
    assert!(
        !terminal.take_damage_flag(),
        "taking it reports the output once"
    );
}

/// The whole point of the pending-damage flag. An animation frame projects
/// over content that has not moved, and the terminal damages the cursor's
/// row on every read, so asking it would name that row and buy a
/// reprojection and a re-upload of it at the frame rate.
#[test]
fn a_projection_over_unchanged_content_reports_nothing_dirty() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    terminal.advance(b"ab");
    terminal.project(&mut grid);

    let (_cursor, _scroll, damage) = terminal.project(&mut grid);
    assert!(
        matches!(&damage, Damage::Partial(rows) if rows.is_empty()),
        "a second projection with nothing between them names no row",
    );
}

/// A scrollback move damages the whole screen without any output arriving,
/// so the flag is not set from the parse alone.
///
/// A move away from the bottom reports that damage whole, the pinned
/// viewport having no slide to make. The return runs the slide and compare,
/// so what it must show is rows rewritten rather than the empty set an
/// uncollected frame comes back with.
#[test]
fn a_scrollback_move_still_collects_damage() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    terminal.advance(b"one\r\ntwo\r\nthree\r\nfour\r\n");
    terminal.project(&mut grid);

    terminal.scroll_display(2);
    let (_cursor, _scroll, damage) = terminal.project(&mut grid);
    assert!(matches!(damage, Damage::Full), "the scrolled view repaints");

    terminal.scroll_to_bottom();
    let (_cursor, _scroll, damage) = terminal.project(&mut grid);
    let Damage::Partial(rows) = &damage else {
        panic!("the return to the live bottom slides and compares");
    };
    assert!(
        rows.iter().any(|dirty| dirty.is_some()),
        "and rewrites the rows the move changed",
    );
}

#[test]
fn output_a_skipped_projection_missed_lands_in_the_next_one() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);
    terminal.project(&mut grid);

    // The cursor ends on row 1, so row 0 is damaged only by the text. A
    // skip that consumed the damage would leave nothing to say so, since
    // the terminal damages the cursor's row on every read regardless.
    terminal.advance(b"hi\r\n");
    terminal.take_damage_flag();
    let (_cursor, _scroll, damage) = terminal.project(&mut grid);

    assert_eq!(
        (grid.get(0, 0).ch, grid.get(0, 1).ch),
        ('h', 'i'),
        "what the skipped projection did not paint is painted here"
    );
    assert!(
        matches!(&damage, Damage::Partial(rows) if rows[0].is_some()),
        "the row still names itself, the skip having left the damage alone"
    );
}

/// A reused buffer carries the marks of the frame that gave it back, so
/// what it must not do is hand them to the frame that takes it.
#[test]
fn a_reused_row_buffer_is_cleared_and_sized_before_it_is_handed_out() {
    let mut spare = vec![vec![whole_row(4), whole_row(4), whole_row(4)]];

    assert_eq!(
        super::row_bounds(&mut spare, 2),
        [None, None],
        "the marks are gone and the buffer fits the rows asked for"
    );
    assert!(spare.is_empty(), "the buffer handed out left the pool");
    assert_eq!(
        super::row_bounds(&mut spare, 3),
        [None, None, None],
        "an empty pool still answers, by allocating"
    );
}

#[test]
fn only_a_damage_carrying_a_buffer_returns_one() {
    let mut terminal = Terminal::new(2, 4, Theme::default());

    terminal.recycle_damage(Damage::Full);
    assert!(
        terminal.row_flags_spare.is_empty(),
        "a full damage names no rows, so it has no buffer to give back"
    );

    // What a frame that read no damage hands back. It names no rows and
    // holds no allocation, so a pool that took it would answer the next
    // request by allocating anyway, over a real buffer sitting below it.
    terminal.recycle_damage(Damage::Partial(Vec::new()));
    assert!(
        terminal.row_flags_spare.is_empty(),
        "an empty vector has no allocation to lend"
    );

    terminal.recycle_damage(Damage::Partial(vec![whole_row(4); 8]));
    assert_eq!(
        super::row_bounds(&mut terminal.row_flags_spare, 2),
        [None, None],
        "the returned buffer serves the next request"
    );
}

/// A frame gives its buffers back whether or not anything took one, so
/// without a cap an animation over unchanged content grows the pool by that
/// frame's pair for as long as it runs.
///
/// The cap still has to clear two frames' pairs. A frame hands its two back
/// before the next one asks for either, so the pair in flight and the pair
/// coming back coexist, and a cap under four would drop a buffer that is
/// about to be wanted.
#[test]
fn the_pool_holds_two_frames_of_buffers_and_no_more() {
    let mut terminal = Terminal::new(2, 4, Theme::default());

    for _ in 0..4 {
        terminal.recycle_damage(Damage::Partial(vec![whole_row(4); 2]));
    }
    let two_frames = terminal.row_flags_spare.len();

    for _ in 0..40 {
        terminal.recycle_damage(Damage::Partial(vec![whole_row(4); 2]));
    }

    assert_eq!(
        (two_frames, terminal.row_flags_spare.len()),
        (4, 4),
        "two frames' buffers are kept, and nothing past them"
    );
}

#[test]
fn projects_plain_text() {
    let (grid, cursor) = project(2, 4, b"hi");

    assert_eq!(grid.get(0, 0).ch, 'h');
    assert_eq!(grid.get(0, 1).ch, 'i');
    assert_eq!(*grid.get(0, 2), Cell::default());
    assert_eq!(*grid.get(1, 0), Cell::default());
    assert_eq!(
        cursor,
        Cursor {
            row: 0,
            col: 2,
            shape: CursorShape::Block
        }
    );
}

#[test]
fn idle_frame_leaves_clean_rows_untouched() {
    let mut terminal = Terminal::new(3, 4, Theme::default());
    let mut grid = Grid::new(3, 4);
    terminal.advance(b"hi");
    terminal.project(&mut grid);

    // An idle frame with no new input damages only the cursor's line (row
    // 0), so the rows below it stay out of the projection. Sentinel them and
    // confirm the re-projection never touches a clean row.
    grid.get_mut(1, 0).ch = 'Z';
    grid.get_mut(2, 0).ch = 'Z';
    let row1 = *grid.get(1, 0);
    let row2 = *grid.get(2, 0);

    terminal.project(&mut grid);

    assert_eq!(
        *grid.get(1, 0),
        row1,
        "a clean row is untouched by an idle frame"
    );
    assert_eq!(
        *grid.get(2, 0),
        row2,
        "a clean row is untouched by an idle frame"
    );
}

#[test]
fn single_dirty_row_reprojects_only_that_row() {
    let mut terminal = Terminal::new(3, 4, Theme::default());
    let mut grid = Grid::new(3, 4);
    terminal.advance(b"aa\r\nbb\r\ncc");
    terminal.project(&mut grid);

    // Sentinel each row's last cell, then damage only the cursor row (row 2,
    // where "cc" left the cursor) by writing in place so the cursor never
    // leaves it.
    for r in 0..3 {
        grid.get_mut(r, 3).ch = 'Z';
    }
    let row0 = *grid.get(0, 3);
    let row1 = *grid.get(1, 3);

    terminal.advance(b"d");
    terminal.project(&mut grid);

    assert_eq!(grid.get(2, 2).ch, 'd', "the dirty row is re-projected");
    assert_ne!(
        grid.get(2, 3).ch,
        'Z',
        "the dirty row's sentinel is overwritten"
    );
    assert_eq!(*grid.get(0, 3), row0, "an unchanged row keeps its cells");
    assert_eq!(*grid.get(1, 3), row1, "an unchanged row keeps its cells");
}

#[test]
fn projection_without_decoration_change_keeps_every_epoch() {
    let mut terminal = Terminal::new(3, 4, Theme::default());
    let mut grid = Grid::new(3, 4);
    terminal.project(&mut grid);
    let popovers = grid.popovers_epoch();
    let text_runs = grid.text_runs_epoch();
    let minimap = grid.minimap_epoch();

    terminal.project(&mut grid);

    assert_eq!(grid.popovers_epoch(), popovers, "popovers epoch stable");
    assert_eq!(grid.text_runs_epoch(), text_runs, "text-runs epoch stable");
    assert_eq!(grid.minimap_epoch(), minimap, "minimap epoch stable");
}

/// A scene sent again unchanged costs nothing, even though the reset that
/// precedes it emptied every list.
///
/// The wire protocol dedups whole scenes, so a scene differing anywhere
/// arrives in full behind a reset, and the reset raises every dirty flag.
/// Left alone that re-stamps decorations that never moved and damages every
/// row they cover, which is what the renderer's row caches then have to
/// rebuild.
#[test]
fn a_reset_and_identical_redeclare_changes_nothing() {
    let border = BorderCommand {
        top: 0,
        left: 0,
        width: 3,
        height: 2,
        color: [1, 2, 3],
        style: ProtoBorderStyle::Light,
    };

    let mut terminal = Terminal::new(4, 6, Theme::default());
    let mut grid = Grid::new(4, 6);
    terminal.advance(&encode_border(&border));
    terminal.project(&mut grid);
    let _ = terminal.take_decoration_damage();

    let popovers = grid.popovers_epoch();
    let text_runs = grid.text_runs_epoch();
    let minimap = grid.minimap_epoch();

    terminal.advance(&encode_reset());
    terminal.advance(&encode_border(&border));
    terminal.project(&mut grid);

    assert_eq!(grid.popovers_epoch(), popovers, "popovers epoch stable");
    assert_eq!(grid.text_runs_epoch(), text_runs, "text-runs epoch stable");
    assert_eq!(grid.minimap_epoch(), minimap, "minimap epoch stable");

    let Damage::Partial(rows) = terminal.take_decoration_damage() else {
        panic!("decoration damage is always partial");
    };
    assert!(
        !rows.iter().any(|row| row.is_some()),
        "an unchanged scene damages no row: {rows:?}",
    );
}

/// A redeclare that moves one decoration re-stamps that one and leaves the
/// rest alone.
///
/// The reset raises every flag, so this is what distinguishes clearing them
/// on equality from clearing them wholesale.
#[test]
fn a_reset_and_changed_redeclare_restamps_only_what_moved() {
    let border = |top: u16| BorderCommand {
        top,
        left: 0,
        width: 3,
        height: 2,
        color: [1, 2, 3],
        style: ProtoBorderStyle::Light,
    };

    let mut terminal = Terminal::new(6, 6, Theme::default());
    let mut grid = Grid::new(6, 6);
    terminal.advance(&encode_border(&border(0)));
    terminal.project(&mut grid);
    let _ = terminal.take_decoration_damage();

    let popovers = grid.popovers_epoch();
    let minimap = grid.minimap_epoch();

    terminal.advance(&encode_reset());
    terminal.advance(&encode_border(&border(3)));
    terminal.project(&mut grid);

    // Borders carry no epoch of their own, so the categories that do are
    // what shows the other flags were dropped.
    assert_eq!(grid.popovers_epoch(), popovers, "popovers epoch stable");
    assert_eq!(grid.minimap_epoch(), minimap, "minimap epoch stable");

    let Damage::Partial(rows) = terminal.take_decoration_damage() else {
        panic!("decoration damage is always partial");
    };
    assert!(
        rows.iter().any(|row| row.is_some()),
        "the moved border damages the rows it left and arrived on",
    );
}

#[test]
fn a_decoration_change_bumps_only_its_epoch() {
    let mut terminal = Terminal::new(3, 4, Theme::default());
    let mut grid = Grid::new(3, 4);
    terminal.project(&mut grid);
    let popovers = grid.popovers_epoch();
    let text_runs = grid.text_runs_epoch();
    let minimap = grid.minimap_epoch();

    // A real declaration, not just the flag. Raising the flag over an
    // unchanged list is what a scene redeclare does, and that is a no-op.
    terminal.advance(&encode_popover(&PopoverCommand {
        top: 1,
        left: 1,
        width: 2,
        height: 2,
        fill: [10, 20, 30],
        border: [40, 50, 60],
        content_fg: [70, 80, 90],
        scale: 1,
        offset: [0, 0],
        bold: false,
        content: "x".to_owned(),
    }));
    terminal.project(&mut grid);

    assert_eq!(
        grid.popovers_epoch(),
        popovers + 1,
        "the changed decoration bumps its epoch"
    );
    assert_eq!(
        grid.text_runs_epoch(),
        text_runs,
        "an unchanged decoration is left alone"
    );
    assert_eq!(
        grid.minimap_epoch(),
        minimap,
        "an unchanged decoration is left alone"
    );
}

#[test]
fn projects_sgr_color_and_bold() {
    let (grid, _) = project(1, 3, b"\x1b[1;31mX");
    let cell = grid.get(0, 0);

    assert_eq!(cell.ch, 'X');
    assert_eq!(cell.fg, Rgb::new(0xcd, 0x00, 0x00));
    assert!(cell.flags.contains(Flags::BOLD));
}

#[test]
fn projects_underline_style_and_color() {
    let (grid, _) = project(1, 3, b"\x1b[4:3;58:2::0:255:0mU");
    let cell = grid.get(0, 0);

    assert_eq!(cell.ch, 'U');
    assert_eq!(cell.underline, UnderlineStyle::Curly);
    assert_eq!(cell.underline_color, Rgb::new(0, 255, 0));
}

#[test]
fn projects_background_color() {
    let (grid, _) = project(1, 3, b"\x1b[42mY");

    assert_eq!(grid.get(0, 0).bg, Rgb::new(0x00, 0xcd, 0x00));
}

#[test]
fn projects_indexed_color() {
    let (grid, _) = project(1, 2, b"\x1b[38;5;231mZ");

    assert_eq!(grid.get(0, 0).fg, Rgb::new(0xff, 0xff, 0xff));
}

#[test]
fn project_resolves_colors_against_theme() {
    let theme = Theme {
        foreground: Rgb::new(4, 5, 6),
        background: Rgb::new(1, 2, 3),
        cursor: Rgb::new(7, 8, 9),
        ansi: [Rgb::new(10, 11, 12); 16],
    };
    let mut terminal = Terminal::new(1, 4, theme);
    let mut grid = Grid::new(1, 4);

    terminal.advance(b"a\x1b[31mb");
    terminal.project(&mut grid);

    assert_eq!(
        grid.get(0, 0).fg,
        Rgb::new(4, 5, 6),
        "default fg from theme"
    );
    assert_eq!(
        grid.get(0, 0).bg,
        Rgb::new(1, 2, 3),
        "default bg from theme"
    );
    assert_eq!(
        grid.get(0, 1).fg,
        Rgb::new(10, 11, 12),
        "ANSI red from theme palette"
    );
}

#[test]
fn set_theme_recolors_already_written_cells() {
    let before = Theme {
        foreground: Rgb::new(4, 5, 6),
        background: Rgb::new(1, 2, 3),
        cursor: Rgb::new(7, 8, 9),
        ansi: [Rgb::new(10, 11, 12); 16],
    };
    let mut terminal = Terminal::new(1, 4, before);
    let mut grid = Grid::new(1, 4);
    terminal.advance(b"a\x1b[31mb");

    terminal.set_theme(Theme {
        foreground: Rgb::new(40, 50, 60),
        background: Rgb::new(10, 20, 30),
        cursor: Rgb::new(70, 80, 90),
        ansi: [Rgb::new(100, 110, 120); 16],
    });
    terminal.project(&mut grid);

    assert_eq!(
        grid.get(0, 0).fg,
        Rgb::new(40, 50, 60),
        "default fg re-resolves against the new theme"
    );
    assert_eq!(
        grid.get(0, 0).bg,
        Rgb::new(10, 20, 30),
        "default bg re-resolves against the new theme"
    );
    assert_eq!(
        grid.get(0, 1).fg,
        Rgb::new(100, 110, 120),
        "the rebuilt palette recolors an ANSI cell written before the swap"
    );
}

#[test]
fn projects_cursor_position() {
    let (_, cursor) = project(3, 5, b"\x1b[2;3H");

    assert_eq!(
        cursor,
        Cursor {
            row: 1,
            col: 2,
            shape: CursorShape::Block
        }
    );
}

#[test]
fn captures_host_query_responses_for_the_pty() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b[6n");
    assert_eq!(
        terminal.take_responses(),
        b"\x1b[1;1R".to_vec(),
        "cursor position report"
    );

    terminal.advance(b"\x1b[c");
    assert_eq!(
        terminal.take_responses(),
        b"\x1b[?6c".to_vec(),
        "primary device attributes"
    );

    assert!(
        terminal.take_responses().is_empty(),
        "buffer drained after taking"
    );
}

#[test]
fn answers_da1_param_zero_form() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b[0c");
    assert_eq!(
        terminal.take_responses(),
        b"\x1b[?6c".to_vec(),
        "the param-0 DA1 form fish sends is answered like the bare form"
    );
}

#[test]
fn answers_xtversion_query() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b[>0q");
    assert_eq!(terminal.take_responses(), XTVERSION_REPLY.as_bytes());
}

#[test]
fn hello_yields_event_and_replies_when_ident_set() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    let ident = IdentReply {
        pid: 42,
        log_id: "20260718-143000-42".to_string(),
        hostname: "host".to_string(),
        version: "v".to_string(),
        protocol: stoatty_protocol::PROTOCOL_VERSION,
    };
    terminal.set_ident(ident.clone());

    let hello = HelloCommand {
        pid: 99,
        log_id: "20260718-143022-99".to_string(),
        hostname: "remote".to_string(),
        version: "pv".to_string(),
        protocol: stoatty_protocol::PROTOCOL_VERSION,
    };
    terminal.advance(&encode_hello(&hello));

    assert_eq!(terminal.take_events(), vec![TermEvent::Hello(hello)]);
    assert_eq!(terminal.take_responses(), encode_ident_reply(&ident));
}

#[test]
fn hello_yields_event_but_no_reply_without_ident() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    let hello = HelloCommand {
        pid: 99,
        log_id: "20260718-143022-99".to_string(),
        hostname: "remote".to_string(),
        version: "pv".to_string(),
        protocol: stoatty_protocol::PROTOCOL_VERSION,
    };
    terminal.advance(&encode_hello(&hello));

    assert_eq!(terminal.take_events(), vec![TermEvent::Hello(hello)]);
    assert!(
        terminal.take_responses().is_empty(),
        "no reply without an installed ident"
    );
}

#[test]
fn surfaces_title_event() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b]2;hi\x07");
    assert_eq!(terminal.take_events(), vec![TermEvent::Title("hi".into())]);
    assert!(
        terminal.take_events().is_empty(),
        "queue drained after taking"
    );
}

#[test]
fn surfaces_bell_event() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x07");
    assert_eq!(terminal.take_events(), vec![TermEvent::Bell]);
}

#[test]
fn surfaces_clipboard_store_event() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b]52;c;aGk=\x07");
    assert_eq!(
        terminal.take_events(),
        vec![TermEvent::ClipboardStore("hi".into())],
        "OSC 52 payload is base64-decoded"
    );
}

/// The parser's OSC buffer keeps its capacity for the session, so a title no
/// program would send must not reach it. The code and its `;` still do,
/// which the parser reads as one empty argument.
#[test]
fn an_oversize_title_reaches_the_parser_empty() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    let mut seq = b"\x1b]0;".to_vec();
    seq.resize(seq.len() + 1024 * 1024, b'a');
    seq.push(0x07);
    terminal.advance(&seq);

    assert_eq!(
        terminal.take_events(),
        vec![TermEvent::Title(String::new())]
    );
}

/// The parser ends an OSC on a cancel as well, then prints what follows. A
/// cut past the cancel takes that text with it.
#[test]
fn a_cancel_ends_an_oversize_titles_cut_and_the_text_after_prints() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    terminal.esc.set_osc_caps(4, 64);

    terminal.advance(b"\x1b]0;oversized\x18printed");

    assert_eq!(
        terminal.take_events(),
        vec![TermEvent::Title(String::new())]
    );
    let mut grid = Grid::new(4, 8);
    terminal.project(&mut grid);
    let row: String = grid.row(0).iter().map(|cell| cell.ch).collect();
    assert_eq!(row.trim_end(), "printed");
}

/// A clipboard write is the one skipped code with a reason to be large, so
/// its cap is far above the cap a title gets.
#[test]
fn a_clipboard_write_past_the_plain_cap_still_stores() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    let mut seq = b"\x1b]52;c;".to_vec();
    seq.extend_from_slice("QUFB".repeat(256 * 1024).as_bytes());
    seq.push(0x07);
    terminal.advance(&seq);

    assert_eq!(
        terminal.take_events(),
        vec![TermEvent::ClipboardStore("A".repeat(3 * 256 * 1024))],
    );
}

/// A clipboard write that trips its cap a read after it started leaves the
/// parser holding the base64 of the reads before. Any terminator makes the
/// parser dispatch that, and a length in fours decodes to a cut clipboard.
#[test]
fn a_clipboard_write_split_past_its_cap_stores_nothing() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    terminal.esc.set_osc_caps(16, 64);

    terminal.advance(format!("\x1b]52;c;{}", "QUFB".repeat(8)).as_bytes());
    terminal.advance(format!("{}\x07", "QUFB".repeat(10)).as_bytes());
    terminal.advance(b"after");

    assert_eq!(terminal.take_events(), Vec::new());
    let mut grid = Grid::new(4, 8);
    terminal.project(&mut grid);
    let row: String = grid.row(0).iter().map(|cell| cell.ch).collect();
    assert_eq!(row.trim_end(), "after", "the text after the write prints");
}

/// A page's parser holds what earlier reads gave it of a string, as the live
/// one does. The reset has to reach the parser holding the string, or that
/// parser stays inside it and takes the page's text as payload.
#[test]
fn a_carried_overrun_inside_a_fill_resets_the_page_parser() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    terminal.esc.set_osc_caps(8, 64);
    declare_pool(&mut terminal, 0, 2, 8);

    terminal.advance(&encode_fill(&FillCommand { pool: 0, index: 0 }));
    terminal.advance(b"\x1b]0;abcde");
    terminal.advance(b"fghij\x07page");
    terminal.advance(&encode_fill_end());

    let page = pool_page(&terminal, 0, 0);
    let row: String = page.row(0).iter().map(|cell| cell.ch).collect();
    assert_eq!(row.trim_end(), "page");
}

#[test]
fn title_stack_round_trip_restores_saved_title() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b]2;A\x07\x1b[22t\x1b]2;B\x07\x1b[23t");
    assert_eq!(
        terminal.take_events(),
        vec![
            TermEvent::Title("A".into()),
            TermEvent::Title("B".into()),
            TermEvent::Title("A".into()),
        ],
        "push saves A, B replaces it, pop restores A"
    );
}

#[test]
fn answers_osc11_background_query() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b]11;?\x1b\\");
    assert_eq!(
        terminal.take_responses(),
        b"\x1b]11;rgb:0000/0000/0000\x1b\\".to_vec(),
        "OSC 11 answers the default background with the ST terminator echoed"
    );
}

#[test]
fn answers_osc4_palette_query() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b]4;1;?\x07");
    assert_eq!(
        terminal.take_responses(),
        b"\x1b]4;1;rgb:cdcd/0000/0000\x07".to_vec(),
        "OSC 4 answers palette entry 1 (ANSI red) with the BEL terminator echoed"
    );
}

#[test]
fn osc10_query_reflects_override_then_reset() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b]10;#ff0000\x07");
    terminal.advance(b"\x1b]10;?\x07");
    assert_eq!(
        terminal.take_responses(),
        b"\x1b]10;rgb:ffff/0000/0000\x07".to_vec(),
        "the OSC 10 override wins over the theme foreground"
    );

    terminal.advance(b"\x1b]110\x07");
    terminal.advance(b"\x1b]10;?\x07");
    assert_eq!(
        terminal.take_responses(),
        b"\x1b]10;rgb:cccc/cccc/cccc\x07".to_vec(),
        "OSC 110 reset restores the theme foreground"
    );
}

/// The window clear reads `default_background`, so an OSC 11 override has
/// to reach it or the gutter past the cell grid strands the old color.
#[test]
fn default_background_follows_the_osc11_override() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    let themed = Theme::default().background;

    assert_eq!(
        terminal.default_background(),
        themed,
        "with no override the theme's background stands"
    );

    terminal.advance(b"\x1b]11;#ff0000\x07");
    assert_eq!(
        terminal.default_background(),
        Rgb::new(0xff, 0, 0),
        "a program's OSC 11 wins over the theme"
    );

    terminal.advance(b"\x1b]111\x07");
    assert_eq!(
        terminal.default_background(),
        themed,
        "OSC 111 hands the theme's background back"
    );
}

#[test]
fn default_cursor_follows_the_osc12_override() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    let themed = Theme::default().cursor;

    terminal.advance(b"\x1b]12;#00ff00\x07");
    assert_eq!(terminal.default_cursor(), Rgb::new(0, 0xff, 0));

    terminal.advance(b"\x1b]112\x07");
    assert_eq!(terminal.default_cursor(), themed);
}

#[test]
fn answers_text_area_pixel_query() {
    let mut terminal = Terminal::new(24, 80, Theme::default());

    terminal.advance(b"\x1b[14t");
    assert!(
        terminal.take_responses().is_empty(),
        "no reply until the cell pixel size is known"
    );

    terminal.set_cell_pixels(8, 16);
    terminal.advance(b"\x1b[14t");
    assert_eq!(
        terminal.take_responses(),
        b"\x1b[4;384;640t".to_vec(),
        "CSI 14 t reports 24 lines * 16px by 80 cols * 8px"
    );
}

#[test]
fn tracks_bracketed_paste_mode() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    assert!(!terminal.bracketed_paste(), "off by default");

    terminal.advance(b"\x1b[?2004h");
    assert!(terminal.bracketed_paste(), "DECSET 2004 enables it");

    terminal.advance(b"\x1b[?2004l");
    assert!(!terminal.bracketed_paste(), "DECRST 2004 disables it");
}

#[test]
fn tracks_focus_reporting_mode() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    assert!(!terminal.report_focus_in_out(), "off by default");

    terminal.advance(b"\x1b[?1004h");
    assert!(terminal.report_focus_in_out(), "DECSET 1004 enables it");

    terminal.advance(b"\x1b[?1004l");
    assert!(!terminal.report_focus_in_out(), "DECRST 1004 disables it");
}

#[test]
fn surfaces_osc9_notification() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b]9;build done\x07");
    assert_eq!(
        terminal.take_events(),
        vec![TermEvent::Notification {
            title: None,
            body: "build done".into()
        }]
    );
}

/// A build tool writes ConEmu's progress report ten to twenty times a
/// second. Read as a notification it would put `4;1;50` on the desktop at
/// that rate, each one a forked helper process.
#[test]
fn an_osc_9_progress_report_is_not_a_notification() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b]9;4;1;50\x07");
    assert_eq!(terminal.take_events(), vec![]);
}

/// ConEmu's subcommands run from one to twelve, so an OSC 9 whose first
/// field falls either side of that is a notification like any other.
#[test]
fn an_osc_9_body_outside_the_subcommand_range_still_notifies() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b]9;0;files changed\x07");
    terminal.advance(b"\x1b]9;13;files changed\x07");
    assert_eq!(
        terminal.take_events(),
        vec![
            TermEvent::Notification {
                title: None,
                body: "0;files changed".into()
            },
            TermEvent::Notification {
                title: None,
                body: "13;files changed".into()
            },
        ]
    );
}

#[test]
fn surfaces_osc777_notification_keeping_semicolons_in_body() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b]777;notify;Done;a;b;c\x07");
    assert_eq!(
        terminal.take_events(),
        vec![TermEvent::Notification {
            title: Some("Done".into()),
            body: "a;b;c".into()
        }],
        "OSC 777 splits kind/title/body but keeps later semicolons in the body"
    );
}

#[test]
fn ignores_non_notify_osc777() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b]777;precmd;ignored\x07");
    assert!(
        terminal.take_events().is_empty(),
        "only the notify kind of OSC 777 delivers"
    );
}

#[test]
fn does_not_mistake_other_private_csi_for_xtversion() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    // modifyOtherKeys (`CSI > 4 ; 1 m`) is a `>`-prefixed CSI that does not
    // end in `q`, so it must not draw an XTVERSION reply.
    terminal.advance(b"\x1b[>4;1m");
    assert!(terminal.take_responses().is_empty());
}

/// A host holds things on a program's behalf while it runs full screen, and
/// leaving the alternate screen is when it has to let them go. Reporting the
/// enter as well would have the host drop them the moment the program
/// started.
#[test]
fn leaving_the_alternate_screen_is_reported_once() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b[?1049h");
    assert_eq!(terminal.take_events(), [], "entering reports nothing");

    terminal.advance(b"\x1b[?1049l");
    assert_eq!(terminal.take_events(), [TermEvent::AltScreenLeft]);

    terminal.advance(b"\x1b[?1049l");
    assert_eq!(
        terminal.take_events(),
        [],
        "leaving a screen already left reports nothing"
    );
}

/// The mode is polled rather than watched for as `1049l`, which is what
/// catches the ways out that never name it. A crashed program leaves a shell
/// that resets rather than one that politely closes the screen it opened.
#[test]
fn a_reset_out_of_the_alternate_screen_is_a_leave() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b[?1049h");
    terminal.take_events();

    terminal.advance(b"\x1bc");
    assert_eq!(terminal.take_events(), [TermEvent::AltScreenLeft]);
}

#[test]
fn answers_fish_startup_handshake() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    // fish's startup burst: kitty-keyboard query (unanswered by default),
    // XTVERSION, then DA1 as its sentinel. The replies come back in order.
    terminal.advance(b"\x1b[?u\x1b[>0q\x1b[0c");

    let mut expected = XTVERSION_REPLY.as_bytes().to_vec();
    expected.extend_from_slice(b"\x1b[?6c");
    assert_eq!(terminal.take_responses(), expected);
}

#[test]
fn xtversion_query_split_across_advances_is_answered() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(b"\x1b[>0");
    assert!(terminal.take_responses().is_empty(), "query incomplete");

    terminal.advance(b"q");
    assert_eq!(terminal.take_responses(), XTVERSION_REPLY.as_bytes());
}

#[test]
fn project_reports_rows_scrolled() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    // Four lines into a two-row screen push the top two off into history.
    terminal.advance(b"a\r\nb\r\nc\r\nd");
    let (_, scrolled, _) = terminal.project(&mut grid);
    assert_eq!(scrolled, 2, "rows scrolled into history");

    // A projection with no new output reports no scroll.
    let (_, scrolled, _) = terminal.project(&mut grid);
    assert_eq!(scrolled, 0, "no further scroll");
}

/// A line of output damages the whole terminal, but the screen only moved
/// up, so the projection has to come back naming the rows that actually
/// differ rather than all of them.
#[test]
fn a_scroll_reports_only_the_rows_that_changed() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    let mut grid = Grid::new(4, 8);

    terminal.advance(b"aaa\r\nbbb\r\nccc\r\nddd");
    terminal.project(&mut grid);

    // One more line scrolls the screen up by one. Rows zero to two now
    // hold what rows one to three held, and only the bottom row is new.
    terminal.advance(b"\r\neee");
    let (_, scrolled, damage) = terminal.project(&mut grid);

    assert_eq!(scrolled, 1, "the screen moved up by one row");
    let Damage::Partial(rows) = damage else {
        panic!("a scroll must not report the whole screen damaged");
    };
    assert_eq!(
        rows,
        vec![None, None, None, whole_row(8)],
        "only the row the scroll exposed is rewritten",
    );

    let row_text = |row: usize| grid.row(row).iter().map(|cell| cell.ch).collect::<String>();
    assert_eq!(
        row_text(0),
        "bbb     ",
        "the slide moved row one to row zero"
    );
    assert_eq!(row_text(3), "eee     ", "the new row holds the new output");
}

/// The alternate screen has no scrollback to grow, so a scroll there
/// reports no movement at all and the shift has to be found by probe.
///
/// This is what vim, less, and tmux scroll through, and without the probe
/// every one of their scrolls re-uploads the whole screen.
#[test]
fn an_alt_screen_scroll_reports_only_the_rows_that_changed() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    let mut grid = Grid::new(4, 8);

    // Enter the alternate screen, then fill it.
    terminal.advance(b"\x1b[?1049h");
    terminal.advance(b"aaa\r\nbbb\r\nccc\r\nddd");
    let (_, _, damage) = terminal.project(&mut grid);
    assert!(matches!(damage, Damage::Partial(_)), "the fill lands first");

    terminal.advance(b"\r\neee");
    let (_, scrolled, damage) = terminal.project(&mut grid);

    assert_eq!(scrolled, 0, "the alternate screen grows no history");
    let Damage::Partial(rows) = damage else {
        panic!("an alt-screen scroll must not report the whole screen damaged");
    };
    assert_eq!(
        rows,
        vec![None, None, None, whole_row(8)],
        "only the row the scroll exposed is rewritten",
    );

    let row_text = |row: usize| grid.row(row).iter().map(|cell| cell.ch).collect::<String>();
    assert_eq!(
        row_text(0),
        "bbb     ",
        "the probe slid row one to row zero"
    );
    assert_eq!(row_text(3), "eee     ", "the new row holds the new output");
}

/// A scroll region starting below the top line grows no scrollback either,
/// and it moves only the rows inside the region.
#[test]
fn a_region_scroll_below_the_top_reports_only_its_rows() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    let mut grid = Grid::new(4, 8);

    terminal.advance(b"aaa\r\nbbb\r\nccc\r\nddd");
    terminal.project(&mut grid);

    // Lines two to four scroll among themselves. A newline at the region's
    // bottom moves them up and leaves line one alone.
    terminal.advance(b"\x1b[2;4r\x1b[4;1H\r\neee");
    let (_, scrolled, damage) = terminal.project(&mut grid);

    assert_eq!(scrolled, 0, "a region below the top grows no history");
    let Damage::Partial(rows) = damage else {
        panic!("a region scroll must not report the whole screen damaged");
    };
    assert_eq!(
        rows,
        vec![None, whole_row(8), whole_row(8), whole_row(8)],
        "the row outside the region is left alone",
    );

    let row_text = |row: usize| grid.row(row).iter().map(|cell| cell.ch).collect::<String>();
    assert_eq!(
        (row_text(0), row_text(1), row_text(3)),
        (
            "aaa     ".to_string(),
            "ccc     ".to_string(),
            "eee     ".to_string()
        ),
        "the region moved up under an untouched first row",
    );
}

/// A cell changing in place damages its own columns, and the projection
/// carries those through rather than collapsing them to the row.
///
/// A spinner, a clock, or a progress cell is the case this is for. Its row
/// otherwise costs a projection of every column on every repaint.
#[test]
fn a_one_cell_change_reports_the_columns_it_touched() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    let mut grid = Grid::new(4, 8);

    terminal.advance(b"aaaaaaaa\r\nbbbbbbbb\r\ncccccccc\r\ndddddddd");
    terminal.project(&mut grid);

    // Park the cursor on row 2, so the move damages that row and the write
    // that follows damages one column of it.
    terminal.advance(b"\x1b[3;5H");
    terminal.project(&mut grid);

    terminal.advance(b"X");
    let (_, _, damage) = terminal.project(&mut grid);

    let Damage::Partial(rows) = &damage else {
        panic!("one written cell must not report the whole screen damaged");
    };
    assert_eq!(
        rows,
        &vec![None, None, Some((4, 5)), None],
        "the bounds name the written cell and the column the cursor moved to",
    );
    assert_eq!(
        damage.columns(2, 8),
        Some((4, 5)),
        "and read back as those two columns of the eight",
    );
    assert_eq!(
        grid.row(2).iter().map(|cell| cell.ch).collect::<String>(),
        "ccccXccc",
        "the bounded projection still lands the cell",
    );
}

/// INSERT mode keeps damage permanently full, so a single typed cell used to
/// re-project and re-upload the whole screen on every keystroke.
#[test]
fn an_insert_mode_keystroke_reports_only_its_row() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    let mut grid = Grid::new(4, 8);

    // IRM on, then fill the screen and settle it into the grid.
    terminal.advance(b"\x1b[4h");
    terminal.advance(b"aaa\r\nbbb\r\nccc\r\nddd");
    terminal.project(&mut grid);

    // One character onto row two, which is all that changes.
    terminal.advance(b"\x1b[3;4HX");
    let (_, _, damage) = terminal.project(&mut grid);

    let Damage::Partial(rows) = damage else {
        panic!("a typed cell must not report the whole screen damaged");
    };
    assert_eq!(
        rows,
        vec![None, None, whole_row(8), None],
        "only the row the keystroke landed on is rewritten",
    );
}

/// While scrolled back the viewport is pinned to its content as history
/// grows, so there is no slide to make and the projection stays as it was.
#[test]
fn a_scroll_while_scrolled_back_still_reports_full_damage() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    terminal.advance(b"a\r\nb\r\nc\r\nd");
    terminal.project(&mut grid);
    terminal.scroll_display(1);
    terminal.project(&mut grid);

    terminal.advance(b"\r\ne");
    let (_, scrolled, damage) = terminal.project(&mut grid);

    assert_eq!(scrolled, 0, "a pinned viewport reports no scroll");
    assert!(
        matches!(damage, Damage::Full),
        "and takes the unchanged full-projection path",
    );
}

#[test]
fn scroll_display_moves_the_viewport_into_history_and_back() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    // Four lines into a two-row screen push "a" and "b" into history.
    terminal.advance(b"a\r\nb\r\nc\r\nd");
    terminal.project(&mut grid);
    assert_eq!(
        (grid.get(0, 0).ch, grid.get(1, 0).ch),
        ('c', 'd'),
        "the live view shows the bottom of output"
    );

    terminal.scroll_display(1);
    terminal.project(&mut grid);
    assert_eq!(
        (grid.get(0, 0).ch, grid.get(1, 0).ch),
        ('b', 'c'),
        "scrolling up one line slides the prior line into view"
    );

    terminal.scroll_to_bottom();
    terminal.project(&mut grid);
    assert_eq!(
        (grid.get(0, 0).ch, grid.get(1, 0).ch),
        ('c', 'd'),
        "scroll_to_bottom restores the live view"
    );
}

#[test]
fn project_reports_no_scroll_while_scrolled_back() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    terminal.advance(b"a\r\nb\r\nc\r\nd");
    terminal.project(&mut grid);

    // Scrolled back, output grows history but the pinned view must not ease.
    terminal.scroll_display(1);
    terminal.advance(b"\r\ne\r\nf");
    let (_, scrolled, _) = terminal.project(&mut grid);
    assert_eq!(scrolled, 0, "no auto-scroll while the view is pinned back");

    // Back at the bottom, live growth counts again.
    terminal.scroll_to_bottom();
    terminal.advance(b"\r\ng");
    let (_, scrolled, _) = terminal.project(&mut grid);
    assert!(scrolled > 0, "live growth resumes counting at the bottom");
}

#[test]
fn mode_queries_reflect_terminal_mode() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    // alacritty enables alternate-scroll by default; the rest start off.
    assert!(!terminal.is_alt_screen(), "alt screen off at startup");
    assert!(
        terminal.alternate_scroll(),
        "alternate scroll on by default"
    );
    assert!(!terminal.mouse_mode(), "mouse reporting off at startup");
    assert!(!terminal.sgr_mouse(), "sgr mouse off at startup");

    // Enter the alt screen (1049), enable mouse click reporting (1000) and
    // SGR encoding (1006), and turn alternate scroll off (1007), so every
    // query flips from its startup value.
    terminal.advance(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?1007l");
    assert!(terminal.is_alt_screen(), "alt screen on");
    assert!(!terminal.alternate_scroll(), "alternate scroll off");
    assert!(terminal.mouse_mode(), "mouse reporting on");
    assert!(terminal.sgr_mouse(), "sgr mouse on");
    assert!(!terminal.mouse_drag(), "click reporting is not drag");
    assert!(!terminal.mouse_motion(), "click reporting is not motion");

    // Button-event tracking (1002) reports motion during a drag. Any-motion
    // tracking (1003) reports motion with no button held.
    terminal.advance(b"\x1b[?1002h");
    assert!(terminal.mouse_drag(), "1002 enables drag reporting");
    assert!(!terminal.mouse_motion(), "1002 is not buttonless motion");

    terminal.advance(b"\x1b[?1003h");
    assert!(terminal.mouse_motion(), "1003 enables motion reporting");
    assert!(terminal.mouse_drag(), "1003 also satisfies drag reporting");
}

#[test]
fn project_skips_undamaged_rows() {
    let mut terminal = Terminal::new(3, 4, Theme::default());
    let mut grid = Grid::new(3, 4);

    terminal.advance(b"AB\r\nCD");
    terminal.project(&mut grid);
    assert_eq!(grid.get(1, 1).ch, 'D');

    grid.get_mut(2, 0).ch = 'Z';

    terminal.advance(b"E");
    let (_, _, damage) = terminal.project(&mut grid);

    assert_eq!(grid.get(1, 2).ch, 'E');
    assert_eq!(grid.get(2, 0).ch, 'Z');
    assert_eq!(grid.get(0, 0).ch, 'A');

    assert!(
        damage.is_dirty(1),
        "the row 'E' landed on is reported damaged"
    );
    assert!(
        !damage.is_dirty(2),
        "the untouched row is undamaged, so its manual 'Z' survives"
    );
}

#[test]
fn project_resizes_grid_to_terminal() {
    let mut terminal = Terminal::new(2, 6, Theme::default());
    let mut grid = Grid::new(1, 1);

    terminal.advance(b"hello");
    terminal.project(&mut grid);

    assert_eq!((grid.rows(), grid.cols()), (2, 6));
    assert_eq!(grid.get(0, 0).ch, 'h');
}

#[test]
fn resize_propagates_to_grid_on_next_project() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    terminal.advance(b"hi");
    terminal.project(&mut grid);
    assert_eq!((grid.rows(), grid.cols()), (2, 4));

    terminal.resize(5, 10);
    terminal.project(&mut grid);

    assert_eq!((grid.rows(), grid.cols()), (5, 10));
}

/// A frame's payload is never handed to the VT parser, which has no use for
/// it. Inside an APC string the parser ignores everything up to the
/// terminator, and a payload cannot hold an ESC for it to act on, so what it
/// does with those bytes is walk past them.
///
/// A synchronized update is what makes that visible. The parser buffers
/// whatever it is given while one is open, so the buffer grows by exactly
/// what reached it.
#[test]
fn a_frame_payload_never_reaches_the_vt_parser() {
    let mut terminal = Terminal::new(2, 8, Theme::default());
    terminal.advance(b"\x1b[?2026h");

    let before = terminal.buffered_sync_bytes();
    terminal.advance(b"\x1b_Gstoatty;border\x1b\\");

    assert_eq!(
        terminal.buffered_sync_bytes() - before,
        4,
        "the introducer and the terminator are passed on, the payload is not",
    );
}

/// A chunk an open update swallowed whole presented nothing, so it warrants
/// no redraw, and a frame inside it does not change that.
///
/// The chunk is longer than what the parser was given, so weighing the
/// buffer against the chunk's own length would count the payload as content
/// that reached the screen and repaint in the middle of an update.
#[test]
fn a_frame_inside_a_synchronized_update_warrants_no_redraw() {
    let mut terminal = Terminal::new(2, 8, Theme::default());
    terminal.advance(b"\x1b[?2026h");

    assert!(
        !terminal.advance(b"\x1b_Gstoatty;border\x1b\\AB"),
        "the update is still buffering, so nothing was presented",
    );
}

#[test]
fn apc_frame_is_not_rendered_as_text() {
    let (grid, _) = project(1, 8, b"\x1b_Gstoatty;border\x1b\\hi");

    assert_eq!(grid.get(0, 0).ch, 'h');
    assert_eq!(grid.get(0, 1).ch, 'i');
    assert_eq!(*grid.get(0, 2), Cell::default());
}

#[test]
fn border_apc_frame_frames_the_region() {
    let frame = encode_border(&BorderCommand {
        top: 0,
        left: 0,
        width: 3,
        height: 2,
        style: ProtoBorderStyle::Light,
        color: [255, 0, 0],
    });

    let mut terminal = Terminal::new(2, 3, Theme::default());
    let mut grid = Grid::new(2, 3);
    terminal.advance(&frame);
    terminal.project(&mut grid);

    let edge = Some(Border {
        style: BorderStyle::Light,
        color: Rgb::new(255, 0, 0),
    });
    assert_eq!(grid.cell_borders(0, 0).top, edge);
    assert_eq!(grid.cell_borders(0, 0).left, edge);
    assert_eq!(grid.cell_borders(1, 2).bottom, edge);
    assert_eq!(grid.cell_borders(1, 2).right, edge);
    assert_eq!(grid.cell_borders(1, 1).top, None);
}

#[test]
fn panel_apc_frame_sets_a_grid_panel() {
    let frame = encode_panel(&PanelCommand {
        top: 1,
        left: 2,
        width: 4,
        height: 3,
        style: ProtoBorderStyle::Rounded,
        border: [40, 50, 60],
        corner_radius: 6,
        fill: Some([10, 20, 30]),
        shadow: ProtoPanelShadow::Drop,
        inset_x: 0,
        above_pools: false,
        anchor: None,
    });

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(&frame);
    terminal.project(&mut grid);

    assert_eq!(
        grid.panels(),
        [Panel {
            top: 1,
            left: 2,
            width: 4,
            height: 3,
            style: BorderStyle::Rounded,
            border: Rgb::new(40, 50, 60),
            corner_radius: 6,
            fill: Some(Rgb::new(10, 20, 30)),
            shadow: PanelShadow::Drop,
            inset_x: 0,
            above_pools: false,
            anchor: None,
            seq: 1,
        }]
    );
}

#[test]
fn panel_apc_frame_carries_the_above_pools_flag() {
    let frame = encode_panel(&PanelCommand {
        top: 1,
        left: 2,
        width: 4,
        height: 3,
        style: ProtoBorderStyle::Rounded,
        border: [40, 50, 60],
        corner_radius: 6,
        fill: Some([10, 20, 30]),
        shadow: ProtoPanelShadow::Drop,
        inset_x: 0,
        above_pools: true,
        anchor: None,
    });

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(&frame);
    terminal.project(&mut grid);

    assert_eq!(
        grid.panels(),
        [Panel {
            top: 1,
            left: 2,
            width: 4,
            height: 3,
            style: BorderStyle::Rounded,
            border: Rgb::new(40, 50, 60),
            corner_radius: 6,
            fill: Some(Rgb::new(10, 20, 30)),
            shadow: PanelShadow::Drop,
            inset_x: 0,
            above_pools: true,
            anchor: None,
            seq: 1,
        }]
    );
}

#[test]
fn reset_clears_accumulated_panels() {
    let panel = encode_panel(&PanelCommand {
        top: 0,
        left: 0,
        width: 3,
        height: 2,
        style: ProtoBorderStyle::Light,
        border: [1, 2, 3],
        corner_radius: 0,
        fill: None,
        shadow: ProtoPanelShadow::None_,
        inset_x: 0,
        above_pools: false,
        anchor: None,
    });

    let mut terminal = Terminal::new(4, 4, Theme::default());
    let mut grid = Grid::new(4, 4);
    terminal.advance(&panel);
    terminal.advance(&encode_reset());
    terminal.project(&mut grid);

    assert!(grid.panels().is_empty());
}

#[test]
fn decoration_seq_stamps_declaration_order_and_resets() {
    let mut stream = encode_panel(&PanelCommand {
        top: 0,
        left: 0,
        width: 3,
        height: 2,
        style: ProtoBorderStyle::Light,
        border: [1, 2, 3],
        corner_radius: 0,
        fill: None,
        shadow: ProtoPanelShadow::None_,
        inset_x: 0,
        above_pools: false,
        anchor: None,
    });
    stream.extend_from_slice(&encode_icon(&IconCommand {
        top: 0,
        left: 0,
        kind: ProtoIconKind::Error,
        color: [1, 2, 3],
        size: 1,
        offset: [0, 0],
    }));
    stream.extend_from_slice(&encode_text_run(&TextRunCommand {
        col: 0,
        row: 0,
        scale: 160,
        color: [1, 2, 3],
        bg: Some([0, 0, 0]),
        follow: 0,
        anchor: None,
        text: "x".to_owned(),
    }));
    stream.extend_from_slice(&encode_bar(&BarCommand {
        x: 0,
        y: 0,
        width: 1,
        height: 16,
        color: [1, 2, 3],
    }));

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(&stream);
    terminal.project(&mut grid);

    assert_eq!(
        (
            grid.panels()[0].seq,
            grid.icons()[0].seq,
            grid.text_runs()[0].seq,
            grid.bars()[0].seq,
        ),
        (1, 2, 3, 4),
        "one counter stamps declaration order across all four decoration lists"
    );

    terminal.advance(&encode_reset());
    terminal.advance(&stream);
    terminal.project(&mut grid);

    assert_eq!(grid.panels()[0].seq, 1, "reset renumbers the seq from 1");
}

#[test]
fn panel_change_damages_its_rows() {
    let panel = encode_panel(&PanelCommand {
        top: 1,
        left: 0,
        width: 3,
        height: 2,
        style: ProtoBorderStyle::Light,
        border: [1, 2, 3],
        corner_radius: 0,
        fill: None,
        shadow: ProtoPanelShadow::None_,
        inset_x: 0,
        above_pools: false,
        anchor: None,
    });

    let mut terminal = Terminal::new(4, 3, Theme::default());
    let mut grid = Grid::new(4, 3);
    terminal.advance(&panel);
    terminal.project(&mut grid);

    let damage = terminal.take_decoration_damage();
    assert!(!damage.is_dirty(0), "row above the panel stays clean");
    assert!(damage.is_dirty(1), "panel top row damaged");
    assert!(damage.is_dirty(2), "panel bottom row damaged");
    assert!(!damage.is_dirty(3), "row below the panel stays clean");
}

#[test]
fn memchr_prescan_preserves_frame_and_query_detection() {
    let frame = encode_border(&BorderCommand {
        top: 0,
        left: 0,
        width: 3,
        height: 2,
        style: ProtoBorderStyle::Light,
        color: [255, 0, 0],
    });
    let edge = Some(Border {
        style: BorderStyle::Light,
        color: Rgb::new(255, 0, 0),
    });

    // An ESC-free chunk takes the memchr fast path; a frame in the next
    // chunk is still detected.
    let mut terminal = Terminal::new(2, 3, Theme::default());
    let mut grid = Grid::new(2, 3);
    terminal.advance(b"hi");
    terminal.advance(&frame);
    terminal.project(&mut grid);
    assert_eq!(
        grid.cell_borders(0, 0).top,
        edge,
        "frame after plain output"
    );

    // A query preceded by plain bytes in one chunk: memchr seeks to the ESC.
    let mut terminal = Terminal::new(2, 8, Theme::default());
    terminal.advance(b"ab\x1b[>0q");
    assert_eq!(
        terminal.take_responses(),
        XTVERSION_REPLY.as_bytes(),
        "query after a plain prefix"
    );
}

#[test]
fn reset_clears_accumulated_borders() {
    let border = encode_border(&BorderCommand {
        top: 0,
        left: 0,
        width: 3,
        height: 2,
        style: ProtoBorderStyle::Light,
        color: [255, 0, 0],
    });

    let mut terminal = Terminal::new(2, 3, Theme::default());
    let mut grid = Grid::new(2, 3);
    terminal.advance(&border);
    terminal.advance(&encode_reset());
    terminal.project(&mut grid);

    assert_eq!(grid.cell_borders(0, 0).top, None);
    assert_eq!(grid.cell_borders(0, 0).left, None);
}

#[test]
fn rounded_border_command_maps_to_rounded_style() {
    let frame = encode_border(&BorderCommand {
        top: 0,
        left: 0,
        width: 2,
        height: 2,
        style: ProtoBorderStyle::Rounded,
        color: [1, 2, 3],
    });

    let mut terminal = Terminal::new(2, 2, Theme::default());
    let mut grid = Grid::new(2, 2);
    terminal.advance(&frame);
    terminal.project(&mut grid);

    assert_eq!(
        grid.cell_borders(0, 0).top,
        Some(Border {
            style: BorderStyle::Rounded,
            color: Rgb::new(1, 2, 3),
        })
    );
}

#[test]
fn scale_apc_frame_claims_the_block() {
    let frame = encode_scale(&ScaleCommand {
        top: 0,
        left: 0,
        scale: 2,
    });

    let mut terminal = Terminal::new(2, 2, Theme::default());
    let mut grid = Grid::new(2, 2);
    terminal.advance(&frame);
    terminal.project(&mut grid);

    assert_eq!(grid.get(0, 0).scale, Scale::Origin(2));
    assert_eq!(grid.get(0, 1).scale, Scale::Covered);
    assert_eq!(grid.get(1, 0).scale, Scale::Covered);
    assert_eq!(grid.get(1, 1).scale, Scale::Covered);
}

#[test]
fn scroll_region_apc_frame_sets_and_replaces_the_region() {
    let region = |offset| ScrollRegion {
        top: 1,
        left: 2,
        width: 4,
        height: 3,
        offset,
    };
    let frame = |offset| {
        encode_scroll_region(&ScrollRegionCommand {
            top: 1,
            left: 2,
            width: 4,
            height: 3,
            offset,
        })
    };

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);

    terminal.advance(&frame(5));
    terminal.project(&mut grid);
    assert_eq!(grid.scroll_region(), Some(region(5)));

    // A later frame replaces the offset rather than adding a second region.
    terminal.advance(&frame(9));
    terminal.project(&mut grid);
    assert_eq!(grid.scroll_region(), Some(region(9)));
}

#[test]
fn popover_apc_frame_sets_a_grid_overlay() {
    let frame = encode_popover(&PopoverCommand {
        top: 1,
        left: 2,
        width: 4,
        height: 3,
        fill: [10, 20, 30],
        border: [40, 50, 60],
        content_fg: [70, 80, 90],
        scale: 2,
        offset: [4, -2],
        bold: true,
        content: "ok".to_owned(),
    });

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(&frame);
    terminal.project(&mut grid);

    assert_eq!(
        grid.overlays(),
        [Overlay {
            top: 1,
            left: 2,
            width: 4,
            height: 3,
            fill: Rgb::new(10, 20, 30),
            border: Rgb::new(40, 50, 60),
            content_fg: Rgb::new(70, 80, 90),
            scale: 2,
            offset: [4, -2],
            bold: true,
            content: "ok".to_owned(),
        }]
    );
}

#[test]
fn popover_content_streams_across_advance_chunks() {
    let frame = encode_popover(&PopoverCommand {
        top: 1,
        left: 2,
        width: 6,
        height: 3,
        fill: [10, 20, 30],
        border: [40, 50, 60],
        content_fg: [70, 80, 90],
        scale: 1,
        offset: [0, 0],
        bold: false,
        content: "streamed".to_owned(),
    });

    // Split just inside the content run (past the open marker's ESC \
    // terminator) so the content arrives across two advance calls, exercising
    // the cross-chunk popover capture.
    let open_end = frame.windows(2).position(|w| w == [0x1b, b'\\']).unwrap() + 2;
    let split = open_end + 3;

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(&frame[..split]);
    terminal.advance(&frame[split..]);
    terminal.project(&mut grid);

    assert_eq!(
        grid.overlays(),
        [Overlay {
            top: 1,
            left: 2,
            width: 6,
            height: 3,
            fill: Rgb::new(10, 20, 30),
            border: Rgb::new(40, 50, 60),
            content_fg: Rgb::new(70, 80, 90),
            scale: 1,
            offset: [0, 0],
            bold: false,
            content: "streamed".to_owned(),
        }]
    );
}

#[test]
fn icon_apc_frame_sets_a_grid_icon() {
    let frame = encode_icon(&IconCommand {
        top: 4,
        left: 1,
        kind: ProtoIconKind::Warning,
        color: [255, 200, 0],
        size: 2,
        offset: [3, 6],
    });

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(&frame);
    terminal.project(&mut grid);

    assert_eq!(
        grid.icons(),
        [Icon {
            top: 4,
            left: 1,
            kind: IconKind::Warning,
            color: Rgb::new(255, 200, 0),
            size: 2,
            offset: [3, 6],
            seq: 1,
        }]
    );
}

#[test]
fn text_run_apc_frame_sets_a_grid_text_run() {
    let frame = encode_text_run(&TextRunCommand {
        col: -8,
        row: 48,
        scale: 192,
        color: [150, 160, 170],
        bg: Some([24, 26, 32]),
        follow: 0,
        anchor: None,
        text: "42".to_owned(),
    });

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(&frame);
    terminal.project(&mut grid);

    assert_eq!(
        grid.text_runs(),
        [TextRun {
            col: -8,
            row: 48,
            scale: 192,
            color: Rgb::new(150, 160, 170),
            bg: Some(Rgb::new(24, 26, 32)),
            follow: 0,
            anchor: None,
            text: "42".into(),
            seq: 1,
        }]
    );
}

/// The capture buffer is handed back and reused, so a run must not read
/// what an earlier one left in it, and a shorter run must not trail the
/// longer one before it.
#[test]
fn runs_captured_through_one_buffer_keep_their_own_text() {
    let run = |text: &str| {
        encode_text_run(&TextRunCommand {
            col: 0,
            row: 0,
            scale: 256,
            color: [1, 2, 3],
            bg: None,
            follow: 0,
            anchor: None,
            text: text.to_owned(),
        })
    };

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(&run("a long first run"));
    terminal.advance(&run("short"));
    terminal.advance(&run(""));
    terminal.project(&mut grid);

    let texts: Vec<&str> = grid
        .text_runs()
        .iter()
        .map(|run| run.text.as_ref())
        .collect();
    assert_eq!(texts, ["a long first run", "short", ""]);
}

/// Valid bytes take a path that skips the lossy rebuild, so the malformed
/// case has to keep working through the fallback.
#[test]
fn a_run_of_invalid_bytes_still_captures_lossily() {
    let mut frame = encode_text_run(&TextRunCommand {
        col: 0,
        row: 0,
        scale: 256,
        color: [1, 2, 3],
        bg: None,
        follow: 0,
        anchor: None,
        text: "ok".to_owned(),
    });
    // Replace the payload's second byte with a lone continuation byte,
    // which no valid sequence can start with.
    let at = frame
        .windows(2)
        .position(|pair| pair == b"ok")
        .expect("the payload is in the frame");
    frame[at + 1] = 0x80;

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(&frame);
    terminal.project(&mut grid);

    assert_eq!(
        grid.text_runs()[0].text.as_ref(),
        "o\u{fffd}",
        "the invalid byte becomes a replacement character",
    );
}

/// The open marker alone, with the content and close marker cut off, is what
/// a writer that died mid-capture leaves behind.
fn open_marker_of(frame: &[u8]) -> &[u8] {
    let open_end = frame
        .windows(2)
        .position(|pair| pair == [ESC, b'\\'])
        .expect("the open marker is terminated")
        + 2;
    &frame[..open_end]
}

/// Nothing the writer sends can close a stranded capture, so the cap is the
/// only path back to a painting screen.
#[test]
fn a_capture_past_the_cap_commits_and_restores_live_routing() {
    let frame = encode_popover(&PopoverCommand {
        top: 1,
        left: 2,
        width: 6,
        height: 3,
        fill: [10, 20, 30],
        border: [40, 50, 60],
        content_fg: [70, 80, 90],
        scale: 1,
        offset: [0, 0],
        bold: false,
        content: String::new(),
    });

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(open_marker_of(&frame));
    terminal.advance(&vec![b'x'; MAX_CAPTURE_BYTES]);
    terminal.advance(b"live");
    terminal.project(&mut grid);

    let overlay = match grid.overlays() {
        [overlay] => overlay,
        overlays => panic!("the overrun capture commits one popover, got {overlays:?}"),
    };
    assert_eq!(
        overlay.content.len(),
        MAX_CAPTURE_BYTES,
        "the popover keeps the text captured up to the cap"
    );
    assert_eq!(
        [
            grid.get(0, 0).ch,
            grid.get(0, 1).ch,
            grid.get(0, 2).ch,
            grid.get(0, 3).ch
        ],
        ['l', 'i', 'v', 'e'],
        "bytes after the cap paint the live grid"
    );
}

/// The shell parent keeps writing after its editor dies, and that output has
/// to reach the screen rather than pile up in the abandoned run's text.
#[test]
fn a_stranded_text_run_stops_swallowing_output_at_the_cap() {
    let frame = text_run_frame("");

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(open_marker_of(&frame));
    terminal.advance(&vec![b'.'; MAX_CAPTURE_BYTES]);
    terminal.advance(b"ok");
    terminal.project(&mut grid);

    assert_eq!(
        [grid.get(0, 0).ch, grid.get(0, 1).ch],
        ['o', 'k'],
        "output past the cap paints the live screen"
    );
}

/// A bar frame per `x`, enough of them to run a decoration list past its
/// cap.
fn bar_frames(count: usize) -> Vec<u8> {
    let mut stream = Vec::new();
    for x in 0..count {
        stream.extend_from_slice(&encode_bar(&BarCommand {
            x: x as i16,
            y: 0,
            width: 1,
            height: 1,
            color: [1, 2, 3],
        }));
    }
    stream
}

/// Only a reset empties a decoration list, so an emitter that re-stamps
/// without sending one would otherwise grow it until the session dies.
#[test]
fn live_decorations_past_the_cap_are_dropped_rather_than_grown() {
    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(&bar_frames(MAX_DECORATIONS + 16));
    terminal.project(&mut grid);

    assert_eq!(
        grid.bars().len(),
        MAX_DECORATIONS,
        "the list stops growing at the cap"
    );
    assert_eq!(
        terminal.bar_seq.len(),
        terminal.bars.len(),
        "a dropped bar skips its seq entry, so the pair stays aligned"
    );
    assert_eq!(
        grid.bars()[0].x,
        0,
        "the newest is dropped, so the scene as first stamped survives"
    );
}

/// Pools survive a reset and are retired only by an explicit drop, so a
/// writer looping fresh ids would otherwise hold memory, and per-frame
/// renderer work, for the terminal's lifetime.
#[test]
fn pools_past_the_cap_are_refused_while_an_existing_one_still_resizes() {
    let mut terminal = Terminal::new(8, 8, Theme::default());
    for id in 0..(MAX_POOLS as u32 + 16) {
        declare_pool(&mut terminal, id, 2, 4);
    }

    assert_eq!(
        terminal.pools.len(),
        MAX_POOLS,
        "a fresh id past the cap creates no pool"
    );
    assert!(
        !terminal.pools.contains_key(&(MAX_POOLS as u32)),
        "the refused id is the late one, not an established pool"
    );

    declare_pool(&mut terminal, 0, 4, 6);
    assert_eq!(
        (
            terminal.pools[&0].region.height,
            terminal.pools[&0].region.width
        ),
        (4, 6),
        "a pool that already exists still re-declares at the cap"
    );
}

/// A store is created by any unseen content id and retired only by its own
/// drop, so the count needs the same ceiling the pools have.
#[test]
fn minimap_stores_past_the_cap_are_refused() {
    let mut terminal = Terminal::new(8, 8, Theme::default());
    for content_id in 0..(MAX_MINIMAP_STORES as u32 + 16) {
        terminal.splice_minimap_content(MinimapLinesCommand {
            content_id,
            start: 0,
            removed: 0,
            lines: vec![summary_line()],
        });
    }

    assert_eq!(
        terminal.minimap_contents.len(),
        MAX_MINIMAP_STORES,
        "a fresh content id past the cap creates no store"
    );
}

/// The projection replays each journaled splice into the grid's own stores,
/// so trimming the term's store without trimming the command would leave the
/// grid growing past the cap and diverging from the term.
#[test]
fn a_store_stops_growing_at_the_line_cap_and_the_journal_is_trimmed_with_it() {
    let mut terminal = Terminal::new(8, 8, Theme::default());
    terminal.splice_minimap_content(MinimapLinesCommand {
        content_id: 1,
        start: 0,
        removed: 0,
        lines: vec![summary_line(); MAX_MINIMAP_LINES + 16],
    });

    assert_eq!(
        terminal.minimap_contents[&1].len(),
        MAX_MINIMAP_LINES,
        "the store stops growing at the line cap"
    );
    let journaled = match terminal.minimap_journal.as_slice() {
        [MinimapJournal::Splice(command)] => command.lines.len(),
        journal => panic!("one splice is journaled, got {} entries", journal.len()),
    };
    assert_eq!(
        journaled, MAX_MINIMAP_LINES,
        "the grid replays the same trimmed splice, so its store matches"
    );
}

#[test]
fn insert_room_leaves_the_spliced_store_at_the_cap() {
    assert_eq!(insert_room(0, 0, 0), MAX_MINIMAP_LINES, "an empty store");
    assert_eq!(insert_room(MAX_MINIMAP_LINES, 0, 0), 0, "a full store");
    assert_eq!(
        insert_room(MAX_MINIMAP_LINES, 0, 10),
        10,
        "removing ten makes room for ten"
    );
    // start and the removal end clamp to the length, so an out-of-range
    // splice removes nothing rather than inventing room.
    assert_eq!(
        insert_room(100, 500, 10),
        MAX_MINIMAP_LINES - 100,
        "a start past the end removes nothing"
    );
    assert_eq!(
        insert_room(100, 0, 500),
        MAX_MINIMAP_LINES,
        "a removal past the end clears the store"
    );
}

/// One shared summary, cloned per line, so a cap-sized splice costs atomic
/// bumps rather than a heap allocation each.
fn summary_line() -> LineSummary {
    Arc::from([MinimapRun {
        start_col: 0,
        len: 1,
        class: 1,
        weight: 255,
    }])
}

/// Page decorations accumulate on the open fill rather than the live lists,
/// so they need the same ceiling.
#[test]
fn page_decorations_past_the_cap_are_dropped() {
    let mut terminal = Terminal::new(2, 4, Theme::default());

    declare_pool(&mut terminal, 0, 2, 4);
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(&bar_frames(MAX_DECORATIONS + 16));
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let (_runs, bars, _polylines) = terminal.pools[&0]
        .page_pool
        .page_decorations(0)
        .expect("page buffered");
    assert_eq!(
        bars.len(),
        MAX_DECORATIONS,
        "the page's bar list stops growing at the cap"
    );
}

/// A window drag during an editor refill resizes mid-fill. Closing the fill
/// there would route the rest of the page to the live parser, painting page
/// content across the screen.
#[test]
fn a_resize_mid_fill_discards_the_page_instead_of_painting_it_live() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    declare_pool(&mut terminal, 0, 4, 8);
    terminal.advance(&encode_fill(&FillCommand { pool: 0, index: 0 }));
    terminal.advance(b"page");

    terminal.resize(4, 10);
    terminal.advance(b"LEAK");
    terminal.advance(&encode_fill_end());

    let mut grid = Grid::new(4, 10);
    terminal.project(&mut grid);
    let row_text =
        |grid: &Grid, row: usize| grid.row(row).iter().map(|cell| cell.ch).collect::<String>();
    assert_eq!(
        row_text(&grid, 0).trim_end(),
        "",
        "page bytes after the resize never reach the live screen"
    );
    assert!(
        terminal.pools[&0].page_pool.page_decorations(0).is_none(),
        "the abandoned page is discarded rather than committed onto the slot"
    );

    terminal.advance(b"ok");
    terminal.project(&mut grid);
    assert_eq!(
        row_text(&grid, 0).trim_end(),
        "ok",
        "ordinary output paints the live screen once the fill ends"
    );
}

#[test]
fn bar_apc_frame_sets_a_grid_bar() {
    let frame = encode_bar(&BarCommand {
        x: -4,
        y: 32,
        width: 3,
        height: 16,
        color: [220, 50, 47],
    });

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(&frame);
    terminal.project(&mut grid);

    assert_eq!(
        grid.bars(),
        [Bar {
            x: -4,
            y: 32,
            width: 3,
            height: 16,
            color: Rgb::new(220, 50, 47),
            seq: 1,
        }]
    );
}

#[test]
fn line_layout_shifts_a_bound_component_past_an_expansion() {
    // Line 1 is three rows tall, so its two extra rows push logical line 3
    // down to physical row 5 (80 sixteenths).
    let layout = encode_line_layout(&LineLayoutCommand {
        heights: vec![1, 3, 1],
    });
    let run = encode_text_run(&TextRunCommand {
        col: 0,
        row: 48,
        scale: 256,
        color: [150, 160, 170],
        bg: Some([0, 0, 0]),
        follow: 0,
        anchor: None,
        text: "4".to_owned(),
    });
    let bar = encode_bar(&BarCommand {
        x: 0,
        y: 48,
        width: 2,
        height: 16,
        color: [220, 50, 47],
    });

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(&layout);
    terminal.advance(&run);
    terminal.advance(&bar);
    terminal.project(&mut grid);

    assert_eq!(grid.text_runs()[0].row, 80, "run shifts down two rows");
    assert_eq!(grid.bars()[0].y, 80, "bar shifts down two rows");
}

#[test]
fn border_re_stamps_on_a_vt_damaged_row() {
    let frame = encode_border(&BorderCommand {
        top: 0,
        left: 0,
        width: 3,
        height: 2,
        style: ProtoBorderStyle::Light,
        color: [255, 0, 0],
    });
    let edge = Some(Border {
        style: BorderStyle::Light,
        color: Rgb::new(255, 0, 0),
    });

    let mut terminal = Terminal::new(2, 3, Theme::default());
    let mut grid = Grid::new(2, 3);
    terminal.advance(&frame);
    terminal.project(&mut grid);
    assert_eq!(grid.cell_borders(0, 0).top, edge);

    // Writing text damages row 0, so its cells are reset; the border must be
    // re-stamped even though no new border command arrived.
    terminal.advance(b"X");
    terminal.project(&mut grid);
    assert_eq!(grid.get(0, 0).ch, 'X');
    assert_eq!(
        grid.cell_borders(0, 0).top,
        edge,
        "border re-stamped on the damaged row"
    );
}

/// A border over rows the projection left alone keeps its edges.
///
/// Those rows were not reprojected, so nothing reset them and the stamp still
/// stands. Skipping them is what stops a keystroke re-stamping every region on
/// the grid, and the window must come out as a full re-stamp would leave it.
#[test]
fn a_border_clear_of_the_damaged_rows_keeps_its_edges() {
    let frame = encode_border(&BorderCommand {
        top: 2,
        left: 0,
        width: 3,
        height: 2,
        style: ProtoBorderStyle::Light,
        color: [255, 0, 0],
    });
    let edge = Some(Border {
        style: BorderStyle::Light,
        color: Rgb::new(255, 0, 0),
    });

    let mut terminal = Terminal::new(4, 3, Theme::default());
    let mut grid = Grid::new(4, 3);
    terminal.advance(&frame);
    terminal.project(&mut grid);
    assert_eq!(
        (grid.cell_borders(2, 0).top, grid.cell_borders(3, 0).bottom),
        (edge, edge),
        "the declared border stamps on arrival",
    );

    // Text on row 0 damages that row alone, well clear of the border's rows.
    terminal.advance(b"X");
    terminal.project(&mut grid);

    assert_eq!(
        (
            grid.get(0, 0).ch,
            grid.cell_borders(2, 0).top,
            grid.cell_borders(3, 0).bottom,
            grid.cell_borders(2, 2).right,
        ),
        ('X', edge, edge, edge),
        "the border survives a projection that never touched its rows",
    );
}

#[test]
fn text_runs_reresolve_when_only_the_line_layout_changes() {
    let run = encode_text_run(&TextRunCommand {
        col: 0,
        row: 48,
        scale: 256,
        color: [150, 160, 170],
        bg: Some([0, 0, 0]),
        follow: 0,
        anchor: None,
        text: "4".to_owned(),
    });

    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(&encode_line_layout(&LineLayoutCommand {
        heights: vec![1, 3, 1],
    }));
    terminal.advance(&run);
    terminal.project(&mut grid);
    assert_eq!(grid.text_runs()[0].row, 80, "run shifts past the tall line");

    // Flatten the layout without re-sending the run; it depends on the layout
    // so it must re-resolve to its unshifted row.
    terminal.advance(&encode_line_layout(&LineLayoutCommand {
        heights: vec![1, 1, 1],
    }));
    terminal.project(&mut grid);
    assert_eq!(
        grid.text_runs()[0].row,
        48,
        "run re-resolves when the layout flattens"
    );
}

#[test]
fn resize_re_applies_decorations() {
    let frame = encode_border(&BorderCommand {
        top: 0,
        left: 0,
        width: 3,
        height: 2,
        style: ProtoBorderStyle::Light,
        color: [255, 0, 0],
    });
    let edge = Some(Border {
        style: BorderStyle::Light,
        color: Rgb::new(255, 0, 0),
    });

    let mut terminal = Terminal::new(2, 3, Theme::default());
    let mut grid = Grid::new(2, 3);
    terminal.advance(&frame);
    terminal.project(&mut grid);

    // A resize clears the grid, so the border must be re-applied even with no
    // new command.
    terminal.resize(4, 6);
    terminal.project(&mut grid);
    assert_eq!((grid.rows(), grid.cols()), (4, 6));
    assert_eq!(
        grid.cell_borders(0, 0).top,
        edge,
        "border re-applied after resize"
    );
}

#[test]
fn idle_projection_keeps_grid_level_decorations() {
    let icon = encode_icon(&IconCommand {
        top: 1,
        left: 1,
        kind: ProtoIconKind::Warning,
        color: [255, 200, 0],
        size: 2,
        offset: [0, 0],
    });

    let mut terminal = Terminal::new(4, 4, Theme::default());
    let mut grid = Grid::new(4, 4);
    terminal.advance(&icon);
    terminal.project(&mut grid);
    assert_eq!(grid.icons().len(), 1);

    // A projection with no new command and no damage skips re-applying the
    // icon, but the grid's list persists.
    terminal.project(&mut grid);
    assert_eq!(grid.icons().len(), 1, "icon survives an idle projection");
}

#[test]
fn synchronized_update_buffers_until_esu() {
    let mut terminal = Terminal::new(2, 3, Theme::default());
    let mut grid = Grid::new(2, 3);

    terminal.advance(b"\x1b[?2026h");
    terminal.advance(b"AB");
    assert!(terminal.sync_deadline().is_some(), "update is buffering");
    terminal.project(&mut grid);
    assert_eq!(grid.get(0, 0).ch, ' ', "buffered content not yet on screen");

    terminal.advance(b"\x1b[?2026l");
    assert!(terminal.sync_deadline().is_none(), "ESU ended the update");
    terminal.project(&mut grid);
    assert_eq!(grid.get(0, 0).ch, 'A');
    assert_eq!(grid.get(0, 1).ch, 'B');
}

#[test]
fn flush_synchronized_update_applies_buffered_bytes() {
    let mut terminal = Terminal::new(2, 3, Theme::default());
    let mut grid = Grid::new(2, 3);

    terminal.advance(b"\x1b[?2026hAB");
    assert!(terminal.sync_deadline().is_some());

    terminal.flush_synchronized_update();
    assert!(terminal.sync_deadline().is_none(), "flush ended the update");
    terminal.project(&mut grid);
    assert_eq!(grid.get(0, 0).ch, 'A');
    assert_eq!(grid.get(0, 1).ch, 'B');
}

#[test]
fn advance_reports_no_redraw_while_buffering() {
    let mut terminal = Terminal::new(2, 3, Theme::default());

    assert!(terminal.advance(b"hi"), "normal output warrants a redraw");
    assert!(
        terminal.advance(b"\x1b[?2026h"),
        "the BSU chunk itself is not all buffered"
    );
    assert!(
        !terminal.advance(b"X"),
        "a fully buffered chunk warrants no redraw"
    );
    assert!(
        terminal.advance(b"\x1b[?2026l"),
        "the ESU flush warrants a redraw"
    );
}

/// A `Gstoatty;text_run` frame carrying `text`, so a test can stamp a
/// distinguishable run and read it back off the projected grid.
fn text_run_frame(text: &str) -> Vec<u8> {
    encode_text_run(&TextRunCommand {
        col: 0,
        row: 0,
        scale: 160,
        color: [200, 200, 200],
        bg: Some([0, 0, 0]),
        follow: 0,
        anchor: None,
        text: text.to_owned(),
    })
}

/// The text of each run on the projected grid, in order.
fn run_labels(grid: &Grid) -> Vec<&str> {
    grid.text_runs().iter().map(|run| &*run.text).collect()
}

/// Commit a one-run scene "A", then open a synchronized update and stage a
/// reset plus a replacement run "B" without closing it, asserting the
/// on-screen scene still reads "A" mid-update. The caller then commits it.
fn stage_reset_and_run_under_sync(terminal: &mut Terminal, grid: &mut Grid) {
    terminal.advance(&text_run_frame("A"));
    terminal.project(grid);
    assert_eq!(run_labels(grid), ["A"], "the initial scene commits");

    terminal.advance(b"\x1b[?2026h");
    terminal.advance(&encode_reset());
    terminal.advance(&text_run_frame("B"));
    terminal.project(grid);
    assert_eq!(run_labels(grid), ["A"], "the prior scene holds mid-update");
}

#[test]
fn synchronized_update_commits_staged_decorations_on_esu() {
    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    stage_reset_and_run_under_sync(&mut terminal, &mut grid);

    terminal.advance(b"\x1b[?2026l");
    terminal.project(&mut grid);
    assert_eq!(
        run_labels(&grid),
        ["B"],
        "ESU commits the staged reset and run atomically"
    );
}

#[test]
fn synchronized_update_commits_staged_decorations_on_flush() {
    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    stage_reset_and_run_under_sync(&mut terminal, &mut grid);

    terminal.flush_synchronized_update();
    terminal.project(&mut grid);
    assert_eq!(
        run_labels(&grid),
        ["B"],
        "the timeout flush commits the staged scene"
    );
}

#[test]
fn a_committed_update_keeps_its_staging_capacity() {
    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    stage_reset_and_run_under_sync(&mut terminal, &mut grid);

    terminal.flush_synchronized_update();

    assert!(
        terminal.sync_staged.capacity() > 0,
        "the committed list keeps its allocation for the next update"
    );
}

#[test]
fn advance_defers_redraw_for_decorations_inside_an_update() {
    let mut terminal = Terminal::new(8, 8, Theme::default());

    assert!(
        terminal.advance(b"\x1b[?2026h"),
        "the BSU chunk wakes the main loop"
    );
    assert!(
        !terminal.advance(&text_run_frame("B")),
        "a decoration chunk wholly inside the update warrants no redraw"
    );
    assert!(
        terminal.advance(b"\x1b[?2026l"),
        "the ESU chunk warrants a redraw"
    );
}

/// Declare pool `id` over a `rows` by `cols` region at the origin, so the
/// pool tests have a pool to fill, scroll, and project before driving it.
fn declare_pool(terminal: &mut Terminal, id: u32, rows: u16, cols: u16) {
    terminal.advance(&encode_pool_region(&PoolRegionCommand {
        pool: id,
        top: 0,
        left: 0,
        width: cols,
        height: rows,
        window: 0,
        kind: PoolKind::Grid,
    }));
}

/// The buffered page `index` of pool `id`, panicking if it is not present.
fn pool_page(terminal: &Terminal, id: u32, index: u64) -> &Grid {
    terminal.pools[&id]
        .page_pool
        .page(index)
        .expect("page buffered")
}

fn declare_window_pool(terminal: &mut Terminal, id: u32, window: u32) {
    terminal.advance(&encode_pool_region(&PoolRegionCommand {
        pool: id,
        top: 0,
        left: 0,
        width: 4,
        height: 2,
        window,
        kind: PoolKind::Grid,
    }));
}

#[test]
fn window_bound_pool_is_excluded_from_the_primary_composite() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_pool(&mut terminal, 1, 2, 4);
    declare_window_pool(&mut terminal, 2, 3);

    let primary: Vec<u32> = terminal.pools().iter().map(|view| view.id).collect();
    assert_eq!(
        primary,
        vec![1],
        "the aux pool stays out of the primary list"
    );

    let aux: Vec<u32> = terminal
        .window_pools(3)
        .iter()
        .map(|view| view.id)
        .collect();
    assert_eq!(aux, vec![2], "the aux pool appears under its window");
    assert!(
        terminal.window_pools(9).is_empty(),
        "no pools for an unused window"
    );
}

/// Declare terminal-kind pool `id` over a `rows` by `cols` region at the origin.
fn declare_terminal_pool(terminal: &mut Terminal, id: u32, rows: u16, cols: u16) {
    terminal.advance(&encode_pool_region(&PoolRegionCommand {
        pool: id,
        top: 0,
        left: 0,
        width: cols,
        height: rows,
        window: 0,
        kind: PoolKind::Terminal,
    }));
}

/// Pool `id`'s page grid as its view reports it, the size of the page a fill
/// then paints, and how many cells of that page's first two rows a fill of one
/// full-width line marks.
///
/// The line fits row 0 only when the page paints through a context as wide as
/// the page, so a narrower one wraps it onto row 1.
fn pool_grids(terminal: &mut Terminal, id: u32) -> ((usize, usize), (usize, usize), [usize; 2]) {
    let grid = terminal
        .pools()
        .into_iter()
        .find(|view| view.id == id)
        .expect("pool declared")
        .grid;
    let mut stream = encode_fill(&FillCommand { pool: id, index: 0 });
    stream.extend_from_slice("x".repeat(grid.1).as_bytes());
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let page = pool_page(terminal, id, 0);
    let marked = |row: usize| page.row(row).iter().filter(|cell| cell.ch == 'x').count();
    (grid, (page.rows(), page.cols()), [marked(0), marked(1)])
}

/// A 40 by 10 region of 10 by 20 pixel cells spans 400 by 200 pixels, which
/// holds 50 by 12 terminal cells of 8 by 16.
#[test]
fn a_terminal_pool_sizes_its_pages_at_the_terminal_cell_size() {
    let mut terminal = Terminal::new(24, 80, Theme::default());
    terminal.set_cell_pixels(10, 20);
    terminal.set_terminal_cell_pixels(8, 16);
    declare_terminal_pool(&mut terminal, 1, 10, 40);

    assert_eq!(pool_grids(&mut terminal, 1), ((12, 50), (12, 50), [50, 0]));
}

#[test]
fn a_grid_pool_ignores_the_terminal_cell_size() {
    let mut terminal = Terminal::new(24, 80, Theme::default());
    terminal.set_cell_pixels(10, 20);
    terminal.set_terminal_cell_pixels(8, 16);
    declare_pool(&mut terminal, 1, 10, 40);

    assert_eq!(pool_grids(&mut terminal, 1), ((10, 40), (10, 40), [40, 0]));
}

/// A font step changes the terminal cell size, and a terminal pool re-sizes its
/// pages without the app declaring it again. Its content version moves, so a
/// renderer reads it as changed.
#[test]
fn a_terminal_cell_size_change_resizes_a_terminal_pool_in_place() {
    let mut terminal = Terminal::new(24, 80, Theme::default());
    terminal.set_cell_pixels(10, 20);
    terminal.set_terminal_cell_pixels(8, 16);
    declare_terminal_pool(&mut terminal, 1, 10, 40);
    let before = terminal.pool_content_version(1);

    terminal.set_terminal_cell_pixels(10, 20);
    let after = terminal.pool_content_version(1);

    assert_eq!(
        (pool_grids(&mut terminal, 1), after == before),
        (((10, 40), (10, 40), [40, 0]), false),
        "the pages follow the new cell size and the pool reads as changed",
    );
}

/// A terminal pool composes its one page whole, at its own grid, and composes
/// nothing until that page is buffered.
#[test]
fn a_terminal_pool_composes_its_page_at_its_grid() {
    let mut terminal = Terminal::new(24, 80, Theme::default());
    terminal.set_cell_pixels(10, 20);
    terminal.set_terminal_cell_pixels(8, 16);
    declare_terminal_pool(&mut terminal, 1, 10, 40);
    let mut out = Grid::new(1, 1);
    let composed = |terminal: &Terminal, out: &mut Grid| {
        let composed = terminal.project_terminal_pool(1, out);
        let text: String = (0..out.cols().min(2))
            .map(|col| out.get(0, col).ch)
            .collect();
        (composed, (out.rows(), out.cols()), text)
    };
    let before = composed(&terminal, &mut out);

    let mut stream = encode_fill(&FillCommand { pool: 1, index: 0 });
    stream.extend_from_slice(b"hi");
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    assert_eq!(
        (before, composed(&terminal, &mut out)),
        (
            (false, (1, 1), " ".to_owned()),
            (true, (12, 50), "hi".to_owned())
        ),
        "an unbuffered page leaves the grid alone, and a buffered one fills it",
    );
}

#[test]
fn a_fill_on_a_window_pool_marks_the_window_dirty_once() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_window_pool(&mut terminal, 2, 3);
    terminal.take_events();

    let mut stream = encode_fill(&FillCommand { pool: 2, index: 0 });
    stream.extend_from_slice(b"hi");
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    assert_eq!(terminal.take_events(), vec![TermEvent::WindowDirty(3)]);
}

#[test]
fn window_open_command_surfaces_as_a_term_event() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    let open = WindowOpenCommand {
        window: 2,
        cols: 80,
        rows: 24,
        title: "editor".to_string(),
    };
    terminal.advance(&encode_window_open(&open));

    assert_eq!(terminal.take_events(), vec![TermEvent::WindowOpen(open)]);
}

#[test]
fn config_reload_command_surfaces_as_a_term_event() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(&encode_config_reload());

    assert_eq!(terminal.take_events(), vec![TermEvent::ConfigReload]);
}

#[test]
fn config_reload_survives_an_active_fill() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    terminal.advance(&encode_fill(&FillCommand { pool: 1, index: 0 }));

    terminal.advance(&encode_config_reload());

    assert_eq!(
        terminal.take_events(),
        vec![TermEvent::ConfigReload],
        "a routed command is not swallowed by a fill redirect"
    );
}

#[test]
fn zoom_capture_command_surfaces_as_a_term_event() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(&encode_zoom_capture(true, false));
    terminal.advance(&encode_zoom_capture(false, false));

    assert_eq!(
        terminal.take_events(),
        vec![
            TermEvent::ZoomCapture {
                on: true,
                inband: false
            },
            TermEvent::ZoomCapture {
                on: false,
                inband: false
            }
        ],
        "a claim and its release both reach the host, in order"
    );
}

/// The delivery mode is the whole reason the host hears about the claim at
/// all on a link, so it has to survive the trip rather than being flattened
/// to the socket default on the way through.
#[test]
fn an_inband_claim_reaches_the_host_asking_for_the_pty() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(&encode_zoom_capture(true, true));

    assert_eq!(
        terminal.take_events(),
        vec![TermEvent::ZoomCapture {
            on: true,
            inband: true
        }],
    );
}

#[test]
fn font_step_command_surfaces_as_a_term_event() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    terminal.advance(&encode_font_step(-2));

    assert_eq!(terminal.take_events(), vec![TermEvent::FontStep(-2)]);
}

/// A zoom press acts on the window, not on the frame being composed, so
/// holding it behind a redirect would drop what the user just pressed.
#[test]
fn zoom_commands_survive_an_active_fill() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    terminal.advance(&encode_fill(&FillCommand { pool: 1, index: 0 }));

    terminal.advance(&encode_zoom_capture(true, false));
    terminal.advance(&encode_font_step(1));

    assert_eq!(
        terminal.take_events(),
        vec![
            TermEvent::ZoomCapture {
                on: true,
                inband: false
            },
            TermEvent::FontStep(1)
        ],
        "both route past a fill redirect"
    );
}

/// Pool state is not a live-grid decoration, so holding it behind a redirect
/// only strands the pool and the page grids the drop was retiring.
#[test]
fn a_pool_drop_during_another_pools_fill_still_retires_the_pool() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_pool(&mut terminal, 1, 2, 4);
    declare_pool(&mut terminal, 2, 2, 4);

    terminal.advance(&encode_fill(&FillCommand { pool: 1, index: 0 }));
    terminal.advance(&encode_pool_drop(&PoolDropCommand { pool: 2 }));
    terminal.advance(&encode_fill_end());

    assert_eq!(
        terminal.pools.keys().copied().collect::<Vec<_>>(),
        [1],
        "the drop retires pool 2 while pool 1 paints"
    );
}

/// Dropping the pool a page is painting for reaches an arm that was
/// unreachable while the command itself was held behind the redirect.
#[test]
fn a_pool_drop_of_the_filling_pool_retires_it_without_leaking_the_page() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_pool(&mut terminal, 1, 4, 8);

    terminal.advance(&encode_fill(&FillCommand { pool: 1, index: 0 }));
    terminal.advance(b"page");
    terminal.advance(&encode_pool_drop(&PoolDropCommand { pool: 1 }));
    terminal.advance(b"LEAK");
    terminal.advance(&encode_fill_end());

    assert!(terminal.pools.is_empty(), "the pool is retired");

    let mut grid = Grid::new(4, 8);
    terminal.project(&mut grid);
    let row_text = grid.row(0).iter().map(|cell| cell.ch).collect::<String>();
    assert_eq!(
        row_text.trim_end(),
        "",
        "page bytes after the drop never reach the live screen"
    );
}

/// Whether pool `id`'s page `index` holds a committed page.
fn page_committed(terminal: &Terminal, id: u32, index: u64) -> bool {
    terminal.pools[&id]
        .page_pool
        .page_decorations(index)
        .is_some()
}

/// The re-declare rebuilds the pool's pages, so the context still painting
/// at the old geometry describes a slot that no longer exists.
#[test]
fn a_region_redeclare_mid_fill_leaves_the_page_uncommitted() {
    let mut terminal = Terminal::new(8, 8, Theme::default());
    declare_pool(&mut terminal, 1, 4, 8);

    terminal.advance(&encode_fill(&FillCommand { pool: 1, index: 0 }));
    terminal.advance(b"page");
    declare_pool(&mut terminal, 1, 6, 8);
    terminal.advance(&encode_fill_end());

    assert!(
        !page_committed(&terminal, 1, 0),
        "the page painted for the old region is discarded"
    );
}

/// A fill naming a pool that does not exist yet paints into a viewport-sized
/// fallback that was never meant to land, which only stayed discarded while
/// the pool lookup at commit failed.
#[test]
fn declaring_a_pool_mid_fill_leaves_the_page_uncommitted() {
    let mut terminal = Terminal::new(8, 8, Theme::default());

    terminal.advance(&encode_fill(&FillCommand { pool: 1, index: 0 }));
    terminal.advance(b"page");
    declare_pool(&mut terminal, 1, 4, 8);
    terminal.advance(&encode_fill_end());

    assert!(
        !page_committed(&terminal, 1, 0),
        "the fallback page is discarded rather than landing on the new pool"
    );
}

/// An app re-declares a region routinely, so a declare that moves nothing
/// must not cost the page being painted.
#[test]
fn a_region_redeclare_at_the_same_size_still_commits_the_page() {
    let mut terminal = Terminal::new(8, 8, Theme::default());
    declare_pool(&mut terminal, 1, 4, 8);

    terminal.advance(&encode_fill(&FillCommand { pool: 1, index: 0 }));
    terminal.advance(b"page");
    declare_pool(&mut terminal, 1, 4, 8);
    terminal.advance(&encode_fill_end());

    assert!(
        page_committed(&terminal, 1, 0),
        "an unchanged re-declare rebuilds nothing, so the page still lands"
    );
    assert_eq!(
        (
            pool_page(&terminal, 1, 0).get(0, 0).ch,
            pool_page(&terminal, 1, 0).get(0, 1).ch
        ),
        ('p', 'a'),
        "and it carries the content painted before the declare"
    );
}

/// A scroll held behind a redirect is lost outright, leaving the pool at a
/// position the app has already moved past.
#[test]
fn a_scroll_during_a_fill_still_lands_on_the_pool() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_pool(&mut terminal, 1, 2, 4);

    terminal.advance(&encode_fill(&FillCommand { pool: 1, index: 0 }));
    terminal.advance(&encode_scroll(&ScrollCommand {
        pool: 1,
        page: 3,
        fraction: 0,
    }));
    terminal.advance(&encode_fill_end());

    assert_eq!(
        terminal.pools[&1].scroll_target.page, 3,
        "the scroll applies rather than being dropped"
    );
}

#[test]
fn a_scroll_on_a_primary_pool_marks_nothing_dirty() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_pool(&mut terminal, 1, 2, 4);
    terminal.take_events();

    terminal.advance(&encode_scroll(&ScrollCommand {
        pool: 1,
        page: 3,
        fraction: 0,
    }));

    assert!(
        terminal.take_events().is_empty(),
        "a primary pool raises no window-dirty event"
    );
}

#[test]
fn fill_paints_page_and_spares_live_grid() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    declare_pool(&mut terminal, 0, 2, 4);
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(b"hi");
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let page = pool_page(&terminal, 0, 0);
    assert_eq!((page.get(0, 0).ch, page.get(0, 1).ch), ('h', 'i'));

    terminal.project(&mut grid);
    assert_eq!(
        grid.get(0, 0).ch,
        ' ',
        "page content never reaches the live grid"
    );
}

/// Paint `text` into page `index` of pool 2, which is declared against
/// window 3, and report the pool version and window events it produced.
fn refill(terminal: &mut Terminal, index: u64, text: &[u8]) -> (u64, Vec<TermEvent>) {
    terminal.take_events();
    let mut stream = encode_fill(&FillCommand { pool: 2, index });
    stream.extend_from_slice(text);
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);
    (
        terminal.pool_content_version(2).expect("pool declared"),
        terminal.take_events(),
    )
}

/// A caller watching the version refills every page it buffers whenever the
/// version moves, so most fills repaint bytes that did not change. Each one
/// reported as a change costs a recompose of everything the pool feeds.
#[test]
fn an_identical_refill_moves_neither_the_version_nor_the_window() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_window_pool(&mut terminal, 2, 3);

    let (first, painted) = refill(&mut terminal, 0, b"hi");
    assert_eq!(painted, vec![TermEvent::WindowDirty(3)]);

    let (again, quiet) = refill(&mut terminal, 0, b"hi");
    assert_eq!(again, first, "the same bytes are not a new version of them");
    assert_eq!(quiet, Vec::new(), "and nothing needs repainting");
}

/// A text run reading `text` at the top-left of its page.
fn page_run(text: &str) -> TextRunCommand {
    TextRunCommand {
        col: 0,
        row: 0,
        scale: 160,
        color: [1, 2, 3],
        bg: None,
        follow: 0,
        anchor: None,
        text: text.to_owned(),
    }
}

/// Replace the runs of page `index` of pool 2 with `runs` through a
/// decorations-only scope that also streams `vt`, and report the pool
/// version and window events it produced.
fn redecorate(
    terminal: &mut Terminal,
    index: u64,
    runs: &[&str],
    vt: &[u8],
) -> (u64, Vec<TermEvent>) {
    terminal.take_events();
    let mut stream = Vec::new();
    encode_fill_decorations_scope(&mut stream, 2, index, |out| {
        out.extend_from_slice(vt);
        for text in runs {
            out.extend(encode_text_run(&page_run(text)));
        }
    });
    terminal.advance(&stream);
    (
        terminal.pool_content_version(2).expect("pool declared"),
        terminal.take_events(),
    )
}

/// The texts of the runs buffered on page `index` of pool 2.
fn page_run_texts(terminal: &Terminal, index: u64) -> Vec<String> {
    let (runs, _, _) = terminal.pools[&2]
        .page_pool
        .page_decorations(index)
        .expect("page buffered");
    runs.iter().map(|run| run.text.to_string()).collect()
}

#[test]
fn a_decorations_only_fill_keeps_the_cells_and_replaces_the_runs() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_window_pool(&mut terminal, 2, 3);
    let mut stream = encode_fill(&FillCommand { pool: 2, index: 0 });
    stream.extend_from_slice(b"hi");
    stream.extend(encode_text_run(&page_run("1")));
    stream.extend(encode_fill_end());
    terminal.advance(&stream);
    let filled = terminal.pool_content_version(2).expect("pool declared");

    let (version, dirtied) = redecorate(&mut terminal, 0, &["2"], b"");

    let page = pool_page(&terminal, 2, 0);
    assert_eq!((page.get(0, 0).ch, page.get(0, 1).ch), ('h', 'i'));
    assert_eq!(page_run_texts(&terminal, 0), ["2"], "only the new run");
    assert_ne!(version, filled, "the slot draws something else");
    assert_eq!(dirtied, vec![TermEvent::WindowDirty(3)]);
}

#[test]
fn an_identical_decorations_only_fill_moves_neither_the_version_nor_the_window() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_window_pool(&mut terminal, 2, 3);
    refill(&mut terminal, 0, b"hi");

    let (first, _) = redecorate(&mut terminal, 0, &["2"], b"");
    let (again, quiet) = redecorate(&mut terminal, 0, &["2"], b"");

    assert_eq!(
        (again, quiet),
        (first, Vec::new()),
        "the same runs are not a new version of them"
    );
}

#[test]
fn a_decorations_only_fill_for_an_unbuffered_page_is_dropped() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_window_pool(&mut terminal, 2, 3);
    let (filled, _) = refill(&mut terminal, 0, b"hi");

    let (version, events) = redecorate(&mut terminal, 3, &["2"], b"");

    assert_eq!(
        (version, events),
        (filled, Vec::new()),
        "no cells to draw over, so nothing moved"
    );
    assert!(terminal.pools[&2].page_pool.page_decorations(3).is_none());
}

/// The window stamp moves with what page 0 draws, through a whole fill and
/// a decorations-only one alike, and a repeat of either leaves it.
#[test]
fn a_window_stamp_moves_only_when_its_page_draws_something_else() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_window_pool(&mut terminal, 2, 3);
    let stamp = |terminal: &Terminal| terminal.pool_window_stamp(2, 0, 1);
    assert_eq!(stamp(&terminal), None, "page 0 unbuffered");

    refill(&mut terminal, 0, b"hi");
    let filled = stamp(&terminal).expect("page 0 buffered");
    refill(&mut terminal, 0, b"hi");
    assert_eq!(stamp(&terminal), Some(filled), "the same cells");

    redecorate(&mut terminal, 0, &["2"], b"");
    let redecorated = stamp(&terminal).expect("page 0 buffered");
    redecorate(&mut terminal, 0, &["2"], b"");
    assert_eq!(
        (redecorated > filled, stamp(&terminal)),
        (true, Some(redecorated)),
        "(a new run moves it, the same run again leaves it)",
    );
}

/// The scope parks the context it borrowed without resetting it, so a byte
/// that reached its parser shows up in the next page painted through it.
#[test]
fn vt_inside_a_decorations_only_fill_paints_nothing() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    let mut grid = Grid::new(4, 8);
    declare_window_pool(&mut terminal, 2, 3);
    refill(&mut terminal, 0, b"hi");

    redecorate(&mut terminal, 0, &[], b"xy");
    refill(&mut terminal, 1, b"");

    let cells = |index| {
        let page = pool_page(&terminal, 2, index);
        (page.get(0, 0).ch, page.get(0, 1).ch)
    };
    assert_eq!(cells(0), ('h', 'i'), "the page keeps its cells");
    assert_eq!(cells(1), (' ', ' '), "the next page starts blank");
    terminal.project(&mut grid);
    assert_eq!(
        (grid.get(0, 0).ch, grid.get(0, 1).ch),
        (' ', ' '),
        "and the live grid never sees the bytes"
    );
}

#[test]
fn a_refill_that_paints_something_else_moves_both() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_window_pool(&mut terminal, 2, 3);

    let (first, _) = refill(&mut terminal, 0, b"hi");
    let (second, dirtied) = refill(&mut terminal, 0, b"ho");

    assert_ne!(second, first);
    assert_eq!(dirtied, vec![TermEvent::WindowDirty(3)]);
}

#[test]
fn a_slot_taking_another_page_moves_both_however_it_looks() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_window_pool(&mut terminal, 2, 3);

    let (first, _) = refill(&mut terminal, 0, b"hi");
    // A page a whole capacity along shares page 0's slot, so this lands the
    // same bytes in the same place and only the page index differs. The
    // pool composes by document row through that index, so what the slot
    // answers for moved even though what it holds did not.
    let (second, dirtied) = refill(&mut terminal, PAGE_POOL_CAPACITY as u64, b"hi");

    assert_ne!(second, first, "the slot answers for a different page now");
    assert_eq!(dirtied, vec![TermEvent::WindowDirty(3)]);
}

/// The commit copies the fill's screen a row at a time, mapping each page row to
/// the term line under it, so content on several rows has to land on the matching
/// rows rather than shifted or collapsed onto one.
#[test]
fn a_fill_lands_each_of_its_rows_on_the_matching_page_row() {
    let mut terminal = Terminal::new(3, 4, Theme::default());

    declare_pool(&mut terminal, 0, 3, 4);
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(b"ab\r\ncd\r\nef");
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let page = pool_page(&terminal, 0, 0);
    let painted: Vec<String> = (0..3)
        .map(|row| (0..2).map(|col| page.get(row, col).ch).collect())
        .collect();
    assert_eq!(
        painted,
        ["ab", "cd", "ef"],
        "each painted row lands on its own page row"
    );
}

/// A window drag reports a resize per pixel, and most land on the cell
/// dimensions already held. The shell is signalled only on a real change,
/// so wiping for one of those leaves a half-painted page with nothing to
/// prompt the repaint that would replace it.
#[test]
fn a_resize_to_the_size_already_held_leaves_a_fill_in_progress() {
    let mut terminal = Terminal::new(2, 4, Theme::default());

    declare_pool(&mut terminal, 0, 2, 4);
    terminal.advance(&encode_fill(&FillCommand { pool: 0, index: 3 }));
    terminal.advance(b"ab");

    let version = terminal.pool_content_version(0).expect("pool declared");
    terminal.resize(2, 4);
    assert_eq!(
        terminal.pool_content_version(0),
        Some(version),
        "nothing changed, so nothing is a new version of anything"
    );

    // Committing the page is what advances the version, so this is measured
    // after the comparison above rather than around it.
    terminal.advance(&encode_fill_end());
    let page = pool_page(&terminal, 0, 3);
    assert_eq!(
        (page.get(0, 0).ch, page.get(0, 1).ch),
        ('a', 'b'),
        "the fill kept its page and finished into it"
    );
}

#[test]
fn a_resize_that_changes_either_dimension_still_empties_the_pools() {
    // Whether the buffered page went and whether the version moved.
    let emptied = |rows: usize, cols: usize| {
        let mut terminal = Terminal::new(2, 4, Theme::default());

        declare_pool(&mut terminal, 0, 2, 4);
        terminal.advance(&encode_fill(&FillCommand { pool: 0, index: 3 }));
        terminal.advance(b"ab");
        terminal.advance(&encode_fill_end());

        let version = terminal.pool_content_version(0).expect("pool declared");
        terminal.resize(rows, cols);

        (
            terminal.pools[&0].page_pool.page(3).is_none(),
            terminal.pool_content_version(0) != Some(version),
        )
    };

    assert_eq!(
        [emptied(3, 4), emptied(2, 5)],
        [(true, true), (true, true)],
        "a change in either dimension drops the page and moves the version"
    );
}

#[test]
fn fill_persists_across_advance_calls() {
    let mut terminal = Terminal::new(2, 4, Theme::default());

    declare_pool(&mut terminal, 0, 2, 4);
    terminal.advance(&encode_fill(&FillCommand { pool: 0, index: 3 }));
    terminal.advance(b"ab");
    terminal.advance(&encode_fill_end());

    let page = pool_page(&terminal, 0, 3);
    assert_eq!((page.get(0, 0).ch, page.get(0, 1).ch), ('a', 'b'));
}

#[test]
fn next_fill_marker_auto_commits_the_previous_page() {
    let mut terminal = Terminal::new(2, 4, Theme::default());

    declare_pool(&mut terminal, 0, 2, 4);
    // No fill_end between the two pages: opening the second must commit the
    // first, so a dropped close cannot strand the redirect.
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(b"AA");
    stream.extend_from_slice(&encode_fill(&FillCommand { pool: 0, index: 1 }));
    stream.extend_from_slice(b"BB");
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let page0 = pool_page(&terminal, 0, 0);
    assert_eq!(page0.get(0, 0).ch, 'A');
    let page1 = pool_page(&terminal, 0, 1);
    assert_eq!(page1.get(0, 0).ch, 'B');
}

#[test]
fn reset_commits_an_in_progress_page() {
    let mut terminal = Terminal::new(2, 4, Theme::default());

    declare_pool(&mut terminal, 0, 2, 4);
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 2 });
    stream.extend_from_slice(b"zz");
    stream.extend_from_slice(&encode_reset());
    terminal.advance(&stream);

    let page = pool_page(&terminal, 0, 2);
    assert_eq!((page.get(0, 0).ch, page.get(0, 1).ch), ('z', 'z'));
}

#[test]
fn fill_decoration_does_not_leak_to_the_live_grid() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    declare_pool(&mut terminal, 0, 2, 4);
    // A decoration command inside a page's stream is page-targeted; it must
    // not stamp the live grid.
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(&encode_border(&BorderCommand {
        top: 0,
        left: 0,
        width: 2,
        height: 2,
        style: ProtoBorderStyle::Light,
        color: [255, 0, 0],
    }));
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    terminal.project(&mut grid);
    assert_eq!(
        grid.cell_borders(0, 0).top,
        None,
        "page border spares the live grid"
    );
}

#[test]
fn fill_captures_a_polyline_onto_the_slot() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    declare_pool(&mut terminal, 0, 2, 4);
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(&encode_polyline(&PolylineCommand {
        points: vec![[8, 0], [8, 16], [24, 32]],
        width: 6,
        color: [220, 50, 47],
    }));
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let (_, _, polylines) = terminal.pools[&0]
        .page_pool
        .page_decorations(0)
        .expect("page buffered");
    assert_eq!(
        polylines,
        [Polyline {
            points: vec![[8, 0], [8, 16], [24, 32]],
            width: 6,
            color: Rgb::new(220, 50, 47),
            seq: 0,
        }]
    );

    terminal.project(&mut grid);
    assert!(
        grid.polylines().is_empty(),
        "a page-targeted path spares the live grid"
    );
}

#[test]
fn a_polyline_outside_a_fill_lands_on_the_live_grid() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    terminal.advance(&encode_polyline(&PolylineCommand {
        points: vec![[0, 0], [16, 16]],
        width: 4,
        color: [1, 2, 3],
    }));
    terminal.project(&mut grid);

    assert_eq!(
        grid.polylines(),
        [Polyline {
            points: vec![[0, 0], [16, 16]],
            width: 4,
            color: Rgb::new(1, 2, 3),
            seq: 1,
        }]
    );
}

/// A declared mark reaches the grid carrying its declaration order, which is
/// what the renderer occludes by.
#[test]
fn a_sketch_lands_on_the_live_grid_with_its_seq() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);
    let command = test_sketch(7);

    terminal.advance(&encode_sketch(&command));
    terminal.project(&mut grid);

    assert_eq!(grid.sketches(), [Sketch { command, seq: 1 }]);
}

/// A mark is a decoration, so a reset takes it off the screen the way it
/// takes off a border. An emitter prefixes a reset to every re-stamp, so a
/// mark that survived one would never leave.
#[test]
fn a_reset_empties_the_sketch_list() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    terminal.advance(&encode_sketch(&test_sketch(7)));
    terminal.project(&mut grid);
    assert_eq!(grid.sketches().len(), 1, "the mark is on the grid");

    terminal.advance(&encode_reset());
    terminal.project(&mut grid);

    assert_eq!(grid.sketches(), [], "the reset took it off");
}

/// An emitter re-declares its whole decoration set every frame. A scene that
/// did not change must leave the epoch alone, or a renderer caching on it
/// rebuilds every frame for nothing.
#[test]
fn an_identical_sketch_redeclaration_holds_the_epoch() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    let mut scene = encode_reset();
    scene.extend_from_slice(&encode_sketch(&test_sketch(7)));

    terminal.advance(&scene);
    terminal.project(&mut grid);
    let after_first = grid.sketches_epoch();

    terminal.advance(&scene);
    terminal.project(&mut grid);

    assert_eq!(
        grid.sketches_epoch(),
        after_first,
        "a byte-identical scene re-stamps nothing",
    );
}

/// The two fields a run grew are what tie a label to the mark it belongs to
/// and to the pane it glides with, so both have to survive the projection.
#[test]
fn a_text_run_carries_its_follow_and_anchor_to_the_grid() {
    let mut terminal = Terminal::new(2, 8, Theme::default());
    let mut grid = Grid::new(2, 8);

    terminal.advance(&encode_text_run(&TextRunCommand {
        col: 0,
        row: 0,
        scale: 256,
        color: [1, 2, 3],
        bg: None,
        follow: 7,
        anchor: Some((3, 1.5)),
        text: "label".to_string(),
    }));
    terminal.project(&mut grid);

    let [run] = grid.text_runs() else {
        panic!("one run reaches the grid, got {:?}", grid.text_runs());
    };
    assert_eq!((run.follow, run.anchor), (7, Some((3, 1.5))));
}

/// A mark with every field set, so a round trip that drops one shows up.
fn test_sketch(id: u32) -> SketchCommand {
    SketchCommand {
        id,
        style: SketchStyle {
            color: [220, 50, 47],
            alpha: 200,
            width: 64,
            roughness: 64,
            seed: 99,
        },
        timing: SketchTiming {
            delay_ms: 120,
            duration_ms: 480,
            easing: SketchEasing::EaseOutCubic,
            phase: SketchPhase::Enter,
        },
        shape: SketchShape::Ellipse {
            bounds: SketchBounds {
                x: -16,
                y: 32,
                w: 240,
                h: 48,
            },
            fill: None,
        },
        anchor: Some((3, 2.5)),
    }
}

/// Reprojecting refills each path's point list where it sits, so the points must
/// come out as declared rather than appended to what the last projection left.
///
/// A path list that shrinks must lose its tail too, or a cleared lane would keep
/// drawing.
#[test]
fn reprojected_paths_hold_only_what_is_declared() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    let path = |x: i16| {
        encode_polyline(&PolylineCommand {
            points: vec![[x, 0], [x + 16, 16]],
            width: 4,
            color: [1, 2, 3],
        })
    };
    terminal.advance(&path(0));
    terminal.advance(&path(32));
    terminal.project(&mut grid);

    // A re-sent line layout re-applies the paths without redeclaring them, which
    // is what an editor does on every wrap change.
    terminal.advance(&encode_line_layout(&LineLayoutCommand {
        heights: vec![1, 1],
    }));
    terminal.project(&mut grid);

    assert_eq!(
        grid.polylines()
            .iter()
            .map(|line| line.points.as_slice())
            .collect::<Vec<_>>(),
        [[[0, 0], [16, 16]], [[32, 0], [48, 16]]],
        "a second projection refills the points rather than growing them",
    );

    // A reset clears the declared paths, so the projection has none to place.
    terminal.advance(&encode_reset());
    terminal.project(&mut grid);

    assert!(
        grid.polylines().is_empty(),
        "a path no longer declared leaves no entry behind, found {}",
        grid.polylines().len(),
    );
}

#[test]
fn fill_captures_bar_and_text_run_onto_the_slot() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut grid = Grid::new(2, 4);

    declare_pool(&mut terminal, 0, 2, 4);
    // A bar and a text run streamed inside the page ride onto its slot, not
    // the live grid. The text run's content proves the capture wins over the
    // fill parser.
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(&encode_bar(&BarCommand {
        x: 0,
        y: 16,
        width: 3,
        height: 16,
        color: [220, 50, 47],
    }));
    stream.extend_from_slice(&encode_text_run(&TextRunCommand {
        col: 0,
        row: 16,
        scale: 160,
        color: [150, 160, 170],
        bg: Some([24, 26, 32]),
        follow: 0,
        anchor: None,
        text: "42".to_owned(),
    }));
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let (runs, bars, _) = terminal.pools[&0]
        .page_pool
        .page_decorations(0)
        .expect("page buffered");
    assert_eq!(
        runs,
        [TextRun {
            col: 0,
            row: 16,
            scale: 160,
            color: Rgb::new(150, 160, 170),
            bg: Some(Rgb::new(24, 26, 32)),
            follow: 0,
            anchor: None,
            text: "42".into(),
            seq: 0,
        }]
    );
    assert_eq!(
        bars,
        [Bar {
            x: 0,
            y: 16,
            width: 3,
            height: 16,
            color: Rgb::new(220, 50, 47),
            seq: 0,
        }]
    );

    terminal.project(&mut grid);
    assert!(
        grid.text_runs().is_empty() && grid.bars().is_empty(),
        "page decorations spare the live grid"
    );
}

#[test]
fn project_pool_stamps_translated_decorations_and_culls_off_window() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut out = Grid::new(3, 4);

    declare_pool(&mut terminal, 0, 2, 4);
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(&encode_text_run(&TextRunCommand {
        col: 0,
        row: 0,
        scale: 160,
        color: [1, 2, 3],
        bg: Some([0, 0, 0]),
        follow: 0,
        anchor: None,
        text: "aa".to_owned(),
    }));
    stream.extend_from_slice(&encode_fill(&FillCommand { pool: 0, index: 1 }));
    stream.extend_from_slice(&encode_text_run(&TextRunCommand {
        col: 0,
        row: 0,
        scale: 160,
        color: [4, 5, 6],
        bg: Some([0, 0, 0]),
        follow: 0,
        anchor: None,
        text: "bb".to_owned(),
    }));
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    // Half a page down puts the window over page 0's last row and all of
    // page 1, so page 1's run (page row 0 to window row 1, y 16) is stamped
    // and page 0's (page row 0, above the window) is culled.
    let projected = terminal.project_pool(0, &mut out, 0.5);
    assert_eq!(projected, Some((0.0, 1)));
    assert_eq!(
        out.text_runs(),
        [TextRun {
            col: 0,
            row: 16,
            scale: 160,
            color: Rgb::new(4, 5, 6),
            bg: Some(Rgb::new(0, 0, 0)),
            follow: 0,
            anchor: None,
            text: "bb".into(),
            seq: 0,
        }]
    );
}

/// A gliding pool stamps into the same grid every frame, and the lists it
/// writes are now the ones already there rather than fresh ones. A stamp
/// therefore has to replace what it finds, including a path list left
/// longer by the stamp before it.
#[test]
fn restamping_a_pool_replaces_what_the_last_stamp_left() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    let mut out = Grid::new(3, 4);
    declare_pool(&mut terminal, 0, 2, 4);

    let path = |points: Vec<[i16; 2]>, width: u16| {
        encode_polyline(&PolylineCommand {
            points,
            width,
            color: [1, 2, 3],
        })
    };
    // Page one is filled once and left alone, so the window always holds
    // two paths and only page zero's changes shape between stamps.
    let fill_page_zero = |terminal: &mut Terminal, points: Vec<[i16; 2]>| {
        let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
        stream.extend_from_slice(&path(points, 8));
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);
    };

    let mut stream = encode_fill(&FillCommand { pool: 0, index: 1 });
    stream.extend_from_slice(&path(vec![[48, 0]], 4));
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    fill_page_zero(&mut terminal, vec![[0, 0], [16, 0], [32, 0]]);
    terminal.project_pool(0, &mut out, 0.0);
    let long = out.polylines().to_vec();
    assert_eq!(
        long.iter().map(|p| p.points.len()).collect::<Vec<_>>(),
        [3, 1],
        "page zero's three points, then page one's single one",
    );

    // The shorter path refills the entry the longer one left, so a stale
    // tail point would ride along into it.
    fill_page_zero(&mut terminal, vec![[64, 0]]);
    terminal.project_pool(0, &mut out, 0.0);
    assert_eq!(
        out.polylines()[0].points,
        [[64, 0]],
        "the shortened path holds only its own point",
    );

    // Back to the longer path, which must land what it did the first time.
    fill_page_zero(&mut terminal, vec![[0, 0], [16, 0], [32, 0]]);
    terminal.project_pool(0, &mut out, 0.0);
    assert_eq!(
        out.polylines(),
        long,
        "a restamp is unaffected by the reuse"
    );
}

#[test]
fn refilling_a_slot_clears_its_decorations() {
    let mut terminal = Terminal::new(2, 4, Theme::default());

    declare_pool(&mut terminal, 0, 2, 4);
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(&encode_bar(&BarCommand {
        x: 0,
        y: 0,
        width: 2,
        height: 16,
        color: [1, 2, 3],
    }));
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    // Refilling the same slot with a decoration-free page drops the old bar.
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(b"x");
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let (runs, bars, _) = terminal.pools[&0]
        .page_pool
        .page_decorations(0)
        .expect("page buffered");
    assert!(
        runs.is_empty() && bars.is_empty(),
        "recycled slot drops the previous page's decorations"
    );
}

/// A committed page's VT context is reset and reused for the next page.
///
/// The reset has to clear everything a page leaves behind. That means its
/// cells, where it parked the cursor, the colors it was still painting in,
/// and the decorations it captured.
#[test]
fn a_page_paints_into_a_reused_context_without_the_previous_page() {
    let mut terminal = Terminal::new(2, 4, Theme::default());

    declare_pool(&mut terminal, 0, 2, 4);

    // The first page fills both rows, leaves the cursor on the second one,
    // captures a bar, and closes mid-SGR with a red foreground still set.
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(b"\x1b[31mAAAA\r\nBBBB");
    stream.extend_from_slice(&encode_bar(&BarCommand {
        x: 0,
        y: 0,
        width: 2,
        height: 16,
        color: [1, 2, 3],
    }));
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);
    assert_eq!(pool_page(&terminal, 0, 0).get(1, 0).ch, 'B', "first page");

    // The second page writes one character into a different slot. Anything
    // the reset missed shows up here.
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 1 });
    stream.extend_from_slice(b"z");
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let page = pool_page(&terminal, 0, 1);
    let row = |r: usize| (0..4).map(|c| page.get(r, c).ch).collect::<String>();
    assert_eq!(
        (row(0), row(1)),
        ("z   ".to_string(), "    ".to_string()),
        "the character lands at the origin on an otherwise blank screen",
    );
    assert_ne!(
        page.get(0, 0).fg,
        pool_page(&terminal, 0, 0).get(0, 0).fg,
        "and in the default foreground, not the color the last page left set",
    );

    let (runs, bars, polylines) = terminal.pools[&0]
        .page_pool
        .page_decorations(1)
        .expect("page buffered");
    assert!(
        runs.is_empty() && bars.is_empty() && polylines.is_empty(),
        "and carries none of the previous page's decorations",
    );
}

/// A parked context is sized to the pool it last painted, so it is only good
/// for a page of the same shape. A larger region gets a fresh one, or the page
/// would paint into a screen too small to hold it.
#[test]
fn a_pool_of_another_size_does_not_paint_through_the_parked_context() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    declare_pool(&mut terminal, 0, 2, 4);
    declare_pool(&mut terminal, 1, 4, 8);

    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(b"AAAA");
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    // The last row and column of the wider region lie outside the narrow
    // screen the first page parked.
    let mut stream = encode_fill(&FillCommand { pool: 1, index: 0 });
    stream.extend_from_slice(b"\x1b[4;8Hz");
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    assert_eq!(
        pool_page(&terminal, 1, 0).get(3, 7).ch,
        'z',
        "the wider page paints its far corner",
    );
}

/// Nothing reads what a page's content replies, so a parked context has to be
/// drained anyway. One sink now serves every page, and an undrained one would
/// grow for the life of the terminal.
#[test]
fn a_parked_context_holds_no_replies_from_the_page_it_painted() {
    let mut terminal = Terminal::new(2, 4, Theme::default());

    declare_pool(&mut terminal, 0, 2, 4);

    // DA1 makes the screen reply toward the PTY, and an OSC title queues a
    // listener event.
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(b"\x1b[c\x1b]0;page\x07x");
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let parked = terminal.fill_scratch.as_ref().expect("context parked");
    let (bytes, events) = (parked.responses.take(), parked.responses.take_events());
    assert!(
        bytes.is_empty() && events.is_empty(),
        "parked context holds {} reply bytes and {} events",
        bytes.len(),
        events.len(),
    );
}

/// A page that opens a synchronized update and never closes it leaves the
/// parser buffering. Carried into the next page, that would swallow its
/// content instead of painting it.
#[test]
fn a_page_left_mid_synchronized_update_does_not_hold_the_next_one_back() {
    let mut terminal = Terminal::new(2, 4, Theme::default());

    declare_pool(&mut terminal, 0, 2, 4);

    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(b"\x1b[?2026hA");
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let mut stream = encode_fill(&FillCommand { pool: 0, index: 1 });
    stream.extend_from_slice(b"z");
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    assert_eq!(
        pool_page(&terminal, 0, 1).get(0, 0).ch,
        'z',
        "the second page paints through a parser the first one left syncing",
    );
}

#[test]
fn scroll_command_sets_the_target() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    declare_pool(&mut terminal, 0, 4, 8);
    terminal.advance(&encode_scroll(&ScrollCommand {
        pool: 0,
        page: 9,
        fraction: 16_384,
    }));

    assert_eq!(
        terminal.pools().first().map(|pool| pool.scroll_target),
        Some(DocumentOffset {
            page: 9,
            fraction: 0.25,
        }),
    );
}

#[test]
fn pools_into_clears_prior_contents() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_pool(&mut terminal, 0, 4, 8);

    let mut buf = Vec::new();
    terminal.pools_into(&mut buf);
    assert_eq!(buf.len(), 1, "one declared pool yields one view");

    // A reused buffer is cleared before refilling, so it never accumulates.
    terminal.pools_into(&mut buf);
    assert_eq!(
        buf.len(),
        1,
        "reuse clears the prior view rather than appending"
    );
}

#[test]
fn pool_cursor_exposes_the_anchor_through_pools_into() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_pool(&mut terminal, 0, 4, 8);
    terminal.advance(&encode_pool_cursor(&PoolCursorCommand {
        pool: 0,
        row: 42,
        col: 7,
    }));

    assert_eq!(
        terminal.pools().first().map(|pool| pool.cursor_anchor),
        Some(Some((42, 7))),
    );
}

#[test]
fn pool_drop_clears_the_cursor_anchor() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_pool(&mut terminal, 0, 4, 8);
    terminal.advance(&encode_pool_cursor(&PoolCursorCommand {
        pool: 0,
        row: 42,
        col: 7,
    }));
    terminal.advance(&encode_pool_drop(&PoolDropCommand { pool: 0 }));

    declare_pool(&mut terminal, 0, 4, 8);
    assert_eq!(
        terminal.pools().first().map(|pool| pool.cursor_anchor),
        Some(None),
        "a re-declared pool does not inherit the dropped pool's anchor",
    );
}

#[test]
fn a_cursor_release_takes_the_anchor_off_its_pool_alone() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_pool(&mut terminal, 0, 4, 8);
    declare_pool(&mut terminal, 1, 4, 8);
    terminal.advance(&encode_pool_cursor(&PoolCursorCommand {
        pool: 0,
        row: 42,
        col: 7,
    }));
    terminal.advance(&encode_pool_cursor(&PoolCursorCommand {
        pool: 1,
        row: 9,
        col: 3,
    }));

    terminal.advance(&encode_pool_cursor_release(&PoolCursorReleaseCommand {
        pool: 0,
    }));

    assert_eq!(
        terminal
            .pools()
            .iter()
            .map(|pool| pool.cursor_anchor)
            .collect::<Vec<_>>(),
        [None, Some((9, 3))],
        "the release clears pool 0's anchor and leaves pool 1's",
    );
}

#[test]
fn pool_anchor_exposes_the_host_and_top_through_pools_into() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_pool(&mut terminal, 0, 4, 8);
    terminal.advance(&encode_pool_anchor(&PoolAnchorCommand {
        pool: 0,
        host: 3,
        top_rows: 12.5,
    }));

    assert_eq!(
        terminal.pools().first().map(|pool| pool.anchor),
        Some(Some((3, 12.5))),
    );
}

#[test]
fn pool_drop_clears_the_anchor() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    declare_pool(&mut terminal, 0, 4, 8);
    terminal.advance(&encode_pool_anchor(&PoolAnchorCommand {
        pool: 0,
        host: 3,
        top_rows: 12.5,
    }));
    terminal.advance(&encode_pool_drop(&PoolDropCommand { pool: 0 }));

    declare_pool(&mut terminal, 0, 4, 8);
    assert_eq!(
        terminal.pools().first().map(|pool| pool.anchor),
        Some(None),
        "a re-declared pool does not inherit the dropped pool's anchor",
    );
}

#[test]
fn pool_anchor_for_an_undeclared_pool_is_ignored() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    terminal.advance(&encode_pool_anchor(&PoolAnchorCommand {
        pool: 9,
        host: 1,
        top_rows: 2.0,
    }));

    assert!(
        terminal.pools().is_empty(),
        "a stray anchor creates no pool"
    );
}

#[test]
fn pool_cursor_for_an_undeclared_pool_is_ignored() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    terminal.advance(&encode_pool_cursor(&PoolCursorCommand {
        pool: 9,
        row: 1,
        col: 2,
    }));

    assert!(
        terminal.pools().is_empty(),
        "a stray anchor creates no pool"
    );
}

#[test]
fn reposition_sets_the_target_and_a_one_shot_jump() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    declare_pool(&mut terminal, 0, 4, 8);
    terminal.advance(&encode_reposition(&RepositionCommand {
        pool: 0,
        page: 1_000,
    }));

    assert_eq!(
        terminal.pools().first().map(|pool| pool.scroll_target),
        Some(DocumentOffset {
            page: 1_000,
            fraction: 0.0,
        }),
    );
    assert_eq!(terminal.take_reposition(0), Some(1_000));
    assert_eq!(
        terminal.take_reposition(0),
        None,
        "the jump is consumed once"
    );
}

#[test]
fn project_pool_composes_from_the_pool_with_the_sub_cell_fraction() {
    let mut terminal = Terminal::new(2, 4, Theme::default());

    declare_pool(&mut terminal, 0, 2, 4);
    // Buffer pages 0 and 1 so a 3-row compose at the top has every row.
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    stream.extend_from_slice(&encode_fill(&FillCommand { pool: 0, index: 1 }));
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let mut out = Grid::new(0, 0);
    // 0.25 pages over a 2-row page is half a row: top row 0, half-cell shift.
    let composed = terminal.project_pool(0, &mut out, 0.25);

    assert_eq!(composed, Some((0.5, 0)));
    assert_eq!(
        (out.rows(), out.cols()),
        (3, 4),
        "region height plus a straddle row"
    );
}

/// A region past the viewport is cut down to a bounded one, in the record
/// the renderer reads and in the pages actually built.
///
/// The dimensions come off the wire and each pool eagerly builds several
/// grids of them, so the `u16` maximum asks for hundreds of gigabytes and
/// aborts the process. Catting a crafted file would end the session.
///
/// The stored record and the pages have to agree, since one places the pool
/// on screen and the other holds what is placed.
#[test]
fn a_pool_region_past_the_viewport_is_clamped() {
    let mut terminal = Terminal::new(4, 8, Theme::default());

    let mut stream = encode_pool_region(&PoolRegionCommand {
        pool: 0,
        top: 0,
        left: 0,
        width: u16::MAX,
        height: u16::MAX,
        window: 0,
        kind: PoolKind::Grid,
    });
    stream.extend_from_slice(&encode_fill(&FillCommand { pool: 0, index: 0 }));
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let pool = terminal.pools.get(&0).expect("the pool was declared");
    assert_eq!(
        (pool.region.width, pool.region.height),
        (16, 8),
        "the stored region is cut to twice the viewport",
    );

    let page = pool.page_pool.page(0).expect("the fill committed a page");
    assert_eq!(
        (page.cols(), page.rows()),
        (16, 8),
        "and the page built for it is the same size, not the size asked for",
    );
}

/// A slot outlives the geometry its last page was painted at, so a fill
/// clamped smaller than the slot leaves the rest of it holding that page.
///
/// A pool on a detached window keeps its pages across a viewport resize,
/// which is what makes the two sizes disagree. The fill that lands next is
/// clamped to the shrunken viewport and paints a corner of the slot.
#[test]
fn a_page_smaller_than_its_slot_shows_none_of_the_page_before_it() {
    let mut terminal = Terminal::new(16, 16, Theme::default());
    terminal.advance(&encode_pool_region(&PoolRegionCommand {
        pool: 0,
        top: 0,
        left: 0,
        width: 16,
        height: 16,
        window: 1,
        kind: PoolKind::Grid,
    }));

    let mut stream = encode_fill(&FillCommand { pool: 0, index: 0 });
    for _ in 0..16 {
        stream.extend_from_slice(b"AAAAAAAAAAAAAAAA");
    }
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);
    assert_eq!(
        pool_page(&terminal, 0, 0).get(15, 15).ch,
        'A',
        "the first page has to fill its slot, or this proves nothing",
    );

    terminal.resize(2, 2);

    // Index 5 lands on index 0's slot, painted through a context the resize
    // clamped to four rows and columns.
    let mut stream = encode_fill(&FillCommand { pool: 0, index: 5 });
    stream.extend_from_slice(b"z");
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let page = pool_page(&terminal, 0, 5);
    let stale = (0..page.rows())
        .flat_map(|row| (0..page.cols()).map(move |col| (row, col)))
        .filter(|&(row, col)| page.get(row, col).ch == 'A')
        .collect::<Vec<_>>();
    assert_eq!(stale, Vec::new(), "no cell may keep the page before it");
    assert_eq!(page.get(0, 0).ch, 'z', "and the new page paints its corner");
}

/// A fill into a region declared before the viewport shrank is bounded too.
///
/// The region was in range when it arrived, so the arriving clamp let it
/// through. A resize can leave it far past the viewport afterwards, and the
/// fill context is built per page from those same dimensions.
#[test]
fn a_fill_is_clamped_when_a_resize_shrinks_the_viewport_under_it() {
    let mut terminal = Terminal::new(64, 64, Theme::default());
    terminal.advance(&encode_pool_region(&PoolRegionCommand {
        pool: 0,
        top: 0,
        left: 0,
        width: 64,
        height: 64,
        window: 0,
        kind: PoolKind::Grid,
    }));

    terminal.resize(2, 2);
    terminal.advance(&encode_fill(&FillCommand { pool: 0, index: 0 }));

    let fill = terminal.fill.as_ref().expect("a fill is open");
    assert_eq!(
        (fill.term.columns(), fill.term.screen_lines()),
        (4, 4),
        "the fill context is twice the live viewport, not the stale region",
    );
}

#[test]
fn project_pool_composes_into_the_declared_pool_region() {
    let mut terminal = Terminal::new(2, 4, Theme::default());

    // A 3x2 pool region narrower than the 4-col viewport; buffer pages 0 and
    // 1 so a 3-row compose at the top has every row.
    let mut stream = encode_pool_region(&PoolRegionCommand {
        pool: 0,
        top: 0,
        left: 0,
        width: 3,
        height: 2,
        window: 0,
        kind: PoolKind::Grid,
    });
    stream.extend_from_slice(&encode_fill(&FillCommand { pool: 0, index: 0 }));
    stream.extend_from_slice(&encode_fill(&FillCommand { pool: 0, index: 1 }));
    stream.extend_from_slice(&encode_fill_end());
    terminal.advance(&stream);

    let mut out = Grid::new(0, 0);
    let composed = terminal.project_pool(0, &mut out, 0.25);

    assert_eq!(composed, Some((0.5, 0)));
    assert_eq!(
        (out.rows(), out.cols()),
        (3, 3),
        "region height plus a straddle row, by region width"
    );
}

#[test]
fn project_pool_degrades_when_no_window_is_buffered() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    declare_pool(&mut terminal, 0, 2, 4);

    // Pre-size out to the projection shape (region height + 1 straddle row,
    // by width) and seed sentinels. A degraded projection must leave the
    // caller's held composite intact rather than resizing or half-writing
    // it, so the compositor can keep showing the last good frame.
    let mut out = Grid::new(3, 4);
    for row in 0..out.rows() {
        for col in 0..out.cols() {
            out.get_mut(row, col).ch = 'Z';
        }
    }
    assert_eq!(terminal.project_pool(0, &mut out, 0.0), None);
    let untouched = (0..out.rows()).all(|r| (0..out.cols()).all(|c| out.get(r, c).ch == 'Z'));
    assert!(untouched, "a degraded projection leaves out untouched");
}

/// Stepping one row deeper into history moves the whole window down by a
/// row, so only the older line revealed at the top is actually new.
#[test]
fn a_scrollback_step_reports_only_the_row_it_revealed() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    // a, b, c, d scroll into history; e, f stay on the live screen.
    terminal.advance(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");

    let mut out = Grid::new(0, 0);
    let mut damage = Damage::Full;

    terminal.project_scrollback(&mut out, 1.0, 0, true, &mut damage);
    assert_eq!(
        [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch],
        ['c', 'd', 'e'],
    );

    // One row deeper. The window slides down, so c and d keep their content
    // at rows one and two, and only the newly revealed b is written.
    terminal.project_scrollback(&mut out, 2.0, -1, true, &mut damage);

    let Damage::Partial(rows) = &damage else {
        panic!("a one-row step must not report the whole window damaged");
    };
    assert_eq!(
        rows,
        &vec![whole_row(4), None, None],
        "only the row the step revealed at the top is rewritten",
    );
    assert_eq!(
        [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch],
        ['b', 'c', 'd'],
        "and the window shows the older line above what it already held",
    );
}

/// Live output arriving while the window holds still leaves every row clean.
///
/// The view is pinned to its content as history grows, so the caller raises the
/// offset by the appended rows and reports no movement. Those frames are the
/// common case while a child streams and the user reads back, and reprojecting
/// the window on each would re-upload it at output rate.
#[test]
fn output_under_a_still_window_dirties_nothing() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    // a, b, c, d scroll into history; e, f stay on the live screen.
    terminal.advance(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");

    let mut out = Grid::new(0, 0);
    let mut damage = Damage::Full;

    terminal.project_scrollback(&mut out, 1.0, 0, true, &mut damage);
    let window = |out: &Grid| [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch];
    assert_eq!(window(&out), ['c', 'd', 'e']);

    // One more line of output pushes e into history. The offset rises with it,
    // which is the pin keeping the window on the rows it already showed.
    terminal.advance(b"\r\ng");
    terminal.project_scrollback(&mut out, 2.0, 0, true, &mut damage);

    let Damage::Partial(rows) = &damage else {
        panic!("a still window must not report itself wholly damaged");
    };
    assert_eq!(
        (rows, window(&out)),
        (&vec![None, None, None], ['c', 'd', 'e']),
        "the window holds the same rows, so none is dirty",
    );
}

/// A scrollback projection takes its row flags from the pool a recycled
/// frame filled.
///
/// A slide the VT stood still through reads only the rows it uncovered, so a
/// carried row is taken on trust. Poisoning one is the only way to see the
/// difference between a row compared and found equal and a row never read,
/// and the difference is the whole change.
#[test]
fn a_slide_the_vt_stood_still_through_reads_only_what_it_uncovered() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    terminal.advance(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");

    let mut out = Grid::new(0, 0);
    let mut damage = Damage::Full;
    terminal.project_scrollback(&mut out, 1.0, 0, true, &mut damage);

    // Row zero is carried down to row one by the step below, where the
    // history says 'c'. A frame that read it would put 'c' back.
    out.get_mut(0, 0).ch = 'X';
    terminal.project_scrollback(&mut out, 2.0, -1, false, &mut damage);

    let Damage::Partial(rows) = &damage else {
        panic!("a one-row step must not report the whole window damaged");
    };
    assert_eq!(
        (
            [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch],
            rows.as_slice(),
        ),
        (['b', 'X', 'd'], [whole_row(4), None, None].as_slice()),
        "the uncovered row is read, and the carried ones are left as they slid",
    );

    // Another step, this one with the VT reported changed. The poisoned row
    // slides to the bottom, which a scoped read would leave alone, so only a
    // whole-window read puts it right.
    terminal.project_scrollback(&mut out, 3.0, -1, true, &mut damage);

    let Damage::Partial(rows) = &damage else {
        panic!("a one-row step must not report the whole window damaged");
    };
    assert_eq!(
        (
            [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch],
            rows.as_slice(),
        ),
        (
            ['a', 'b', 'c'],
            [whole_row(4), None, whole_row(4)].as_slice()
        ),
        "a step the VT touched reads every row, and corrects the carried one",
    );
}

/// A rebuild with neither a move nor a VT change names nothing that could
/// have changed, and gets the window read whole rather than not at all.
#[test]
fn a_rebuild_naming_no_change_still_reads_the_window() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    terminal.advance(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");

    let mut out = Grid::new(0, 0);
    let mut damage = Damage::Full;
    terminal.project_scrollback(&mut out, 1.0, 0, true, &mut damage);

    out.get_mut(1, 0).ch = 'X';
    terminal.project_scrollback(&mut out, 1.0, 0, false, &mut damage);

    assert_eq!(
        [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch],
        ['c', 'd', 'e'],
        "the window is read whole, so the row that drifted is put back",
    );
}

/// Which end a slide vacates decides which rows there are to read, and
/// getting it backwards would read the rows that carried and trust the ones
/// that came in blank.
#[test]
fn a_slide_uncovers_the_edge_it_vacated() {
    assert_eq!(
            (
                uncovered_rows(2, 8),
                uncovered_rows(-2, 8),
                uncovered_rows(9, 8),
                uncovered_rows(-9, 8),
            ),
            (6..8, 0..2, 0..8, 0..8),
            "content moving up uncovers the bottom, down uncovers the top, past the height uncovers all",
        );
}

/// Drawing from the pool only saves an allocation while something puts one
/// back, since the pool falls back to a fresh buffer when it is empty. So
/// the round trip is what this checks, not the draw alone.
#[test]
fn a_recycled_scrollback_frame_supplies_the_next_one() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    terminal.advance(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");

    let mut out = Grid::new(0, 0);
    let mut damage = Damage::Partial(Vec::new());
    terminal.project_scrollback(&mut out, 1.0, 0, true, &mut damage);
    assert!(
        matches!(damage, Damage::Partial(_)),
        "the fixture has to reach the diffing path to produce a buffer",
    );

    assert!(
        terminal.row_flags_spare.is_empty(),
        "nothing has been handed back yet",
    );
    terminal.recycle_damage(std::mem::replace(&mut damage, Damage::Partial(Vec::new())));
    assert_eq!(
        terminal.row_flags_spare.len(),
        1,
        "the read frame's buffer went back to the pool",
    );

    terminal.advance(b"\r\ng");
    terminal.project_scrollback(&mut out, 2.0, 0, true, &mut damage);
    assert!(
        terminal.row_flags_spare.is_empty(),
        "the next projection filled its flags into the pooled buffer",
    );
}

/// A move at or past the window height leaves nothing worth comparing against,
/// so the window is reprojected whole and reported wholly damaged.
#[test]
fn a_move_clearing_the_window_reprojects_it_whole() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    terminal.advance(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");

    let mut out = Grid::new(0, 0);
    let mut damage = Damage::Partial(vec![None; 3]);

    // Three rows of window, so a move of three has nothing left to keep.
    terminal.project_scrollback(&mut out, 1.0, 3, true, &mut damage);

    assert!(
        matches!(damage, Damage::Full),
        "a cleared window cannot report rows it kept"
    );
    assert_eq!(
        [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch],
        ['c', 'd', 'e'],
        "and it still holds the rows the offset names",
    );
}

#[test]
fn project_scrollback_composes_a_straddled_history_window() {
    let mut terminal = Terminal::new(2, 4, Theme::default());
    // a, b, c scroll into history; d, e stay on the live screen.
    terminal.advance(b"a\r\nb\r\nc\r\nd\r\ne");

    let mut out = Grid::new(0, 0);

    // At the live bottom nothing is scrolled back: fall back to the live grid.
    assert_eq!(
        terminal.project_scrollback(&mut out, 0.0, 0, true, &mut Damage::Full),
        None
    );

    // One row back: the window is the older straddle row (b) above the
    // offset-1 view (c, d), shifted up a whole row so the straddle hides.
    assert_eq!(
        terminal.project_scrollback(&mut out, 1.0, 0, true, &mut Damage::Full),
        Some(-1.0)
    );
    assert_eq!(
        (out.rows(), out.cols()),
        (3, 4),
        "viewport plus a straddle row"
    );
    assert_eq!(
        [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch],
        ['b', 'c', 'd'],
    );

    // Half a row deeper keeps the same window, shifted by the sub-cell frac.
    assert_eq!(
        terminal.project_scrollback(&mut out, 1.5, 0, true, &mut Damage::Full),
        Some(-0.5)
    );
    assert_eq!(
        [out.get(0, 0).ch, out.get(1, 0).ch, out.get(2, 0).ch],
        ['b', 'c', 'd'],
    );

    // At the oldest line the straddle falls above history and stays blank.
    assert_eq!(
        terminal.project_scrollback(&mut out, 3.0, 0, true, &mut Damage::Full),
        Some(-1.0)
    );
    assert_eq!(*out.get(0, 0), Cell::default(), "no row older than the top");
    assert_eq!([out.get(1, 0).ch, out.get(2, 0).ch], ['a', 'b']);
}

fn light_border(top: u16, height: u16) -> Vec<u8> {
    encode_border(&BorderCommand {
        top,
        left: 0,
        width: 3,
        height,
        style: ProtoBorderStyle::Light,
        color: [255, 0, 0],
    })
}

#[test]
fn border_change_damages_its_rows() {
    let mut terminal = Terminal::new(4, 3, Theme::default());
    let mut grid = Grid::new(4, 3);
    terminal.advance(&light_border(1, 2));
    terminal.project(&mut grid);

    let damage = terminal.take_decoration_damage();
    assert!(!damage.is_dirty(0), "row above the border stays clean");
    assert!(damage.is_dirty(1), "border top row damaged");
    assert!(damage.is_dirty(2), "border bottom row damaged");
    assert!(!damage.is_dirty(3), "row below the border stays clean");
}

#[test]
fn clearing_a_border_damages_its_prior_rows() {
    let mut terminal = Terminal::new(4, 3, Theme::default());
    let mut grid = Grid::new(4, 3);
    terminal.advance(&light_border(1, 2));
    terminal.project(&mut grid);
    terminal.take_decoration_damage();

    terminal.advance(&encode_reset());
    terminal.project(&mut grid);
    let damage = terminal.take_decoration_damage();
    assert!(damage.is_dirty(1), "cleared border's prior top row damaged");
    assert!(
        damage.is_dirty(2),
        "cleared border's prior bottom row damaged"
    );
}

/// A projection that changes no decoration leaves the retained footprint in place
/// rather than rebuilding it, so the erase has to still work across frames that
/// skipped the rebuild.
#[test]
fn clearing_a_border_after_idle_frames_damages_its_prior_rows() {
    let mut terminal = Terminal::new(4, 3, Theme::default());
    let mut grid = Grid::new(4, 3);
    terminal.advance(&light_border(1, 2));
    terminal.project(&mut grid);
    terminal.take_decoration_damage();

    for _ in 0..3 {
        terminal.project(&mut grid);
        terminal.take_decoration_damage();
    }

    terminal.advance(&encode_reset());
    terminal.project(&mut grid);
    let damage = terminal.take_decoration_damage();
    assert!(
        damage.is_dirty(1) && damage.is_dirty(2),
        "the border's rows are erased however many idle frames preceded the clear"
    );
    assert!(
        !damage.is_dirty(0) && !damage.is_dirty(3),
        "rows the border never covered stay clean"
    );
}

#[test]
fn scale_change_damages_its_block_rows() {
    let mut terminal = Terminal::new(4, 3, Theme::default());
    let mut grid = Grid::new(4, 3);
    terminal.advance(&encode_scale(&ScaleCommand {
        top: 1,
        left: 0,
        scale: 2,
    }));
    terminal.project(&mut grid);

    let damage = terminal.take_decoration_damage();
    assert!(!damage.is_dirty(0));
    assert!(damage.is_dirty(1), "scale block top row damaged");
    assert!(damage.is_dirty(2), "scale block bottom row damaged");
    assert!(!damage.is_dirty(3));
}

#[test]
fn unchanged_projection_yields_no_decoration_damage() {
    let mut terminal = Terminal::new(4, 3, Theme::default());
    let mut grid = Grid::new(4, 3);
    terminal.advance(&light_border(1, 2));
    terminal.project(&mut grid);
    terminal.take_decoration_damage();

    terminal.project(&mut grid);
    let damage = terminal.take_decoration_damage();
    assert!(
        (0..4).all(|row| !damage.is_dirty(row)),
        "an unchanged projection damages no decoration rows"
    );
}

#[test]
fn vt_damage_alone_yields_no_decoration_damage() {
    let mut terminal = Terminal::new(4, 3, Theme::default());
    let mut grid = Grid::new(4, 3);
    terminal.advance(&light_border(0, 2));
    terminal.project(&mut grid);
    terminal.take_decoration_damage();

    // Writing text re-stamps the same border onto the reset cells; the border
    // instances are unchanged, so no decoration damage.
    terminal.advance(b"X");
    terminal.project(&mut grid);
    let damage = terminal.take_decoration_damage();
    assert!(
        (0..4).all(|row| !damage.is_dirty(row)),
        "a VT re-stamp leaves the border unchanged"
    );
}

#[test]
#[ignore = "throughput benchmark; run with: cargo test -p stoatty_term --lib -- --ignored advance_plain_throughput"]
fn advance_plain_throughput() {
    let mut terminal = Terminal::new(50, 200, Theme::default());
    let mut buf = Vec::with_capacity(64 * 1024);
    while buf.len() < 64 * 1024 {
        buf.extend_from_slice(b"the quick brown fox jumps over the lazy dog 0123456789\r\n");
    }

    let iterations = 400;
    let start = std::time::Instant::now();
    for _ in 0..iterations {
        terminal.advance(&buf);
    }
    let per = start.elapsed() / iterations;

    eprintln!("advance() {}KB ESC-free: {per:?}/call", buf.len() / 1024);
}

#[test]
fn selection_text_returns_the_dragged_range() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    terminal.advance(b"hello");
    terminal.start_selection(0, 0, false);
    terminal.update_selection(0, 4, true);
    assert_eq!(terminal.selection_text().as_deref(), Some("hello"));
}

#[test]
fn project_inverts_selected_cells() {
    let mut terminal = Terminal::new(4, 8, Theme::default());
    let mut grid = Grid::new(4, 8);
    terminal.advance(b"hello");
    terminal.start_selection(0, 0, false);
    terminal.update_selection(0, 2, true);
    terminal.project(&mut grid);

    for col in 0..=2 {
        assert!(
            grid.get(0, col).flags.contains(Flags::INVERSE),
            "selected col {col} is inverted"
        );
    }
    assert!(
        !grid.get(0, 3).flags.contains(Flags::INVERSE),
        "col past the selection is not inverted"
    );
}

#[test]
fn selection_growth_damages_the_entered_rows() {
    let mut terminal = Terminal::new(6, 8, Theme::default());
    let mut grid = Grid::new(6, 8);
    terminal.advance(b"a\r\nb\r\nc");
    terminal.start_selection(0, 0, false);
    terminal.update_selection(0, 0, true);
    terminal.project(&mut grid);

    // Grow the selection to row 1. That row is neither the anchor nor the
    // cursor row, so it repaints only because the selection reached it.
    terminal.update_selection(1, 0, true);
    let (_, _, damage) = terminal.project(&mut grid);
    assert!(damage.is_dirty(1), "row 1 newly entered the selection");
    assert!(
        !damage.is_dirty(5),
        "a row outside the selection stays clean"
    );
}

#[test]
fn a_drag_inside_one_row_damages_that_row() {
    let mut terminal = Terminal::new(6, 8, Theme::default());
    let mut grid = Grid::new(6, 8);
    terminal.advance(b"abcdef");
    terminal.start_selection(0, 0, false);
    terminal.update_selection(0, 1, true);
    terminal.project(&mut grid);

    // The span is row 0 either way, so only comparing the range notices.
    terminal.update_selection(0, 3, true);
    let (_, _, damage) = terminal.project(&mut grid);
    assert!(damage.is_dirty(0), "the row the drag widened within");
    assert!(!damage.is_dirty(1), "and no row it never reached");
}

#[test]
fn a_shrinking_drag_damages_the_row_it_left() {
    let mut terminal = Terminal::new(6, 8, Theme::default());
    let mut grid = Grid::new(6, 8);
    terminal.advance(b"a\r\nb\r\nc\r\nd");
    terminal.start_selection(0, 0, false);
    terminal.update_selection(3, 0, true);
    terminal.project(&mut grid);

    terminal.update_selection(1, 0, true);
    let (_, _, damage) = terminal.project(&mut grid);
    assert!(damage.is_dirty(2), "row 2 left the selection");
    assert!(damage.is_dirty(3), "and so did row 3");
    assert!(
        damage.is_dirty(1),
        "row 1 is the new end, bounded by a column"
    );
}

#[test]
fn a_tall_selection_growing_by_a_row_leaves_its_middle_alone() {
    let mut terminal = Terminal::new(8, 8, Theme::default());
    let mut grid = Grid::new(8, 8);
    terminal.advance(b"a\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng");
    terminal.start_selection(0, 0, false);
    terminal.update_selection(5, 0, true);
    terminal.project(&mut grid);

    terminal.update_selection(6, 0, true);
    let (_, _, damage) = terminal.project(&mut grid);
    assert!(damage.is_dirty(6), "the row the drag entered");
    assert!(damage.is_dirty(5), "and the row that stopped being the end");
    for row in 1..=4 {
        assert!(
            !damage.is_dirty(row),
            "row {row} was inverted end to end before and after",
        );
    }
}

/// A selection from `(top, left)` to `(bottom, right)`, at display offset
/// zero so its lines read as viewport rows.
fn range(top: i32, left: usize, bottom: i32, right: usize, is_block: bool) -> SelectionRange {
    SelectionRange {
        start: Point::new(Line(top), Column(left)),
        end: Point::new(Line(bottom), Column(right)),
        is_block,
    }
}

fn changed_rows(old: Option<SelectionRange>, new: Option<SelectionRange>) -> Vec<usize> {
    let mut dirty = vec![None; 8];
    mark_selection_change(old, new, 0, 8, 8, &mut dirty);
    dirty
        .iter()
        .enumerate()
        .filter_map(|(row, damage)| damage.is_some().then_some(row))
        .collect()
}

/// Nothing in the terminal builds a block selection today, so this states
/// the rule against the function rather than through a drag.
#[test]
fn a_block_selection_marks_every_row_it_spans() {
    assert_eq!(
        changed_rows(Some(range(1, 0, 4, 1, true)), Some(range(1, 0, 4, 3, true)),),
        vec![1, 2, 3, 4],
        "every row of a block is bounded by the same columns it widened",
    );
}

#[test]
fn a_simple_selection_marks_only_its_edges_and_what_it_crossed() {
    assert_eq!(
        changed_rows(
            Some(range(1, 0, 5, 0, false)),
            Some(range(1, 0, 6, 0, false)),
        ),
        vec![1, 5, 6],
        "the row entered, the row that stopped being the end, and the anchor",
    );
    assert_eq!(
        changed_rows(None, Some(range(2, 0, 3, 0, false))),
        vec![2, 3],
        "a selection appearing marks what it covers",
    );
    assert_eq!(
        changed_rows(Some(range(2, 0, 3, 0, false)), None),
        vec![2, 3],
        "and one leaving marks what it covered",
    );
}

fn minimap_cmd(strip_id: u32, content_id: u32) -> MinimapCommand {
    MinimapCommand {
        top: 0,
        left: 72,
        width: 8,
        height: 40,
        strip_id,
        content_id,
        lines_per_cell: 8,
        max_columns: 120,
        bg: [0, 0, 0, 0],
        thumb: [200, 200, 200, 48],
        thumb_border: [255, 255, 255],
        palette: vec![[0, 0, 0], [1, 2, 3]],
    }
}

fn run(start_col: u8, len: u8, class: u8) -> MinimapRun {
    MinimapRun {
        start_col,
        len,
        class,
        weight: 255,
    }
}

/// The shared summaries a store holds, for stating one either way round.
fn summaries(lines: Vec<Vec<MinimapRun>>) -> Vec<LineSummary> {
    lines.into_iter().map(Into::into).collect()
}

fn splice(content_id: u32, start: u32, removed: u32, lines: Vec<Vec<MinimapRun>>) -> Vec<u8> {
    encode_minimap_lines(&MinimapLinesCommand {
        content_id,
        start,
        removed,
        lines: summaries(lines),
    })
}

#[test]
fn minimap_lines_splices_at_boundaries() {
    let mut terminal = Terminal::new(4, 4, Theme::default());

    terminal.advance(&splice(
        9,
        0,
        0,
        vec![vec![run(0, 2, 1)], vec![run(0, 3, 2)], vec![run(1, 1, 3)]],
    ));
    assert_eq!(
        terminal.minimap_contents[&9].len(),
        3,
        "three lines inserted"
    );

    terminal.advance(&splice(9, 1, 1, vec![vec![run(2, 2, 4)]]));
    assert_eq!(
        terminal.minimap_contents[&9],
        summaries(vec![
            vec![run(0, 2, 1)],
            vec![run(2, 2, 4)],
            vec![run(1, 1, 3)]
        ]),
        "the middle line is replaced in place",
    );

    terminal.advance(&splice(9, 2, 1, vec![]));
    assert_eq!(
        terminal.minimap_contents[&9].len(),
        2,
        "a pure deletion removes the last line",
    );

    terminal.advance(&splice(9, 99, 5, vec![vec![run(0, 1, 0)]]));
    assert_eq!(
        terminal.minimap_contents[&9],
        summaries(vec![
            vec![run(0, 2, 1)],
            vec![run(2, 2, 4)],
            vec![run(0, 1, 0)]
        ]),
        "an out-of-range start clamps and appends",
    );
}

/// A resize keeps the grid's stores, so the projection replays the journal
/// across one the way it does across any other frame.
///
/// Cloning every store instead is what a window drag used to cost, and the
/// splice below is what a replay has to carry that a dropped journal would
/// lose.
#[test]
fn a_resize_replays_the_journal_rather_than_recloning() {
    let mut terminal = Terminal::new(4, 4, Theme::default());
    let mut grid = Grid::new(4, 4);

    terminal.advance(&splice(9, 0, 0, vec![vec![run(0, 2, 1)]]));
    terminal.project(&mut grid);
    assert_eq!(
        grid.minimap_content(9),
        summaries(vec![vec![run(0, 2, 1)]]),
        "the first projection lands the store"
    );

    // A splice and a resize in the same frame. The store the grid keeps is
    // what the replay builds on, so losing either half loses the line.
    terminal.advance(&splice(9, 1, 0, vec![vec![run(2, 2, 4)]]));
    terminal.resize(6, 6);
    terminal.project(&mut grid);

    assert_eq!(
        grid.minimap_content(9),
        summaries(vec![vec![run(0, 2, 1)], vec![run(2, 2, 4)]]),
        "the resized frame carries both the kept line and the spliced one"
    );
}

#[test]
fn reset_clears_minimap_strips_but_keeps_content_and_view() {
    let mut terminal = Terminal::new(4, 4, Theme::default());
    terminal.advance(&encode_minimap(&minimap_cmd(5, 9)));
    terminal.advance(&splice(9, 0, 0, vec![vec![run(0, 2, 1)]]));
    terminal.advance(&encode_minimap_view(&MinimapViewCommand {
        strip_id: 5,
        top_256: 256,
        visible_lines: 20,
    }));
    assert_eq!(terminal.minimaps.len(), 1, "the strip is declared");

    terminal.advance(&encode_reset());

    assert!(
        terminal.minimaps.is_empty(),
        "reset clears the strip declaration",
    );
    assert_eq!(
        terminal.minimap_contents[&9].len(),
        1,
        "the content store survives reset",
    );
    assert!(
        terminal.minimap_views.contains_key(&5),
        "the view survives reset",
    );
}

/// The reset prune only fires when a reset arrives, so a writer that loops
/// strip ids without ever sending one needs a ceiling of its own.
#[test]
fn views_past_the_cap_are_refused_while_an_established_strip_still_advances() {
    let view = |strip_id: u32, top_256: u32| {
        encode_minimap_view(&MinimapViewCommand {
            strip_id,
            top_256,
            visible_lines: 20,
        })
    };

    let mut terminal = Terminal::new(4, 4, Theme::default());
    for strip_id in 0..(MAX_MINIMAP_VIEWS as u32 + 16) {
        terminal.advance(&view(strip_id, 256));
    }

    assert_eq!(
        terminal.minimap_views.len(),
        MAX_MINIMAP_VIEWS,
        "a fresh strip id past the cap creates no view"
    );

    terminal.advance(&view(0, 512));
    assert_eq!(
        terminal.minimap_views[&0].top_256, 512,
        "a strip already drawn keeps advancing its thumb at the cap"
    );
}

/// No command retires a view on its own, so a view whose strip the scene
/// never drew would sit in the map for the terminal's lifetime.
#[test]
fn reset_retires_views_for_strips_the_scene_does_not_declare() {
    let mut terminal = Terminal::new(4, 4, Theme::default());
    terminal.advance(&encode_minimap(&minimap_cmd(5, 9)));
    for strip_id in [5, 7] {
        terminal.advance(&encode_minimap_view(&MinimapViewCommand {
            strip_id,
            top_256: 256,
            visible_lines: 20,
        }));
    }

    terminal.advance(&encode_reset());

    assert_eq!(
        terminal.minimap_views.keys().copied().collect::<Vec<_>>(),
        [5],
        "the undeclared strip's view is retired, the declared one's is kept"
    );
}

/// A real emitter retires a view by simply not drawing its strip again. The
/// reset prefixing the next frame is where the thumb state then goes.
#[test]
fn a_strip_the_scene_stops_declaring_loses_its_view() {
    let mut terminal = Terminal::new(4, 4, Theme::default());
    terminal.advance(&encode_minimap(&minimap_cmd(5, 9)));
    terminal.advance(&encode_minimap_view(&MinimapViewCommand {
        strip_id: 5,
        top_256: 256,
        visible_lines: 20,
    }));
    terminal.advance(&encode_reset());
    assert!(
        terminal.minimap_views.contains_key(&5),
        "the strip was still declared, so its view carries over"
    );

    // The next frame draws no strip at all, so the reset that follows it
    // finds nothing declaring strip 5.
    terminal.advance(&encode_reset());

    assert!(
        terminal.minimap_views.is_empty(),
        "the view goes once the scene stops declaring its strip"
    );
}

#[test]
fn minimap_drop_removes_content_store() {
    let mut terminal = Terminal::new(4, 4, Theme::default());
    terminal.advance(&splice(9, 0, 0, vec![vec![run(0, 2, 1)]]));
    assert!(terminal.minimap_contents.contains_key(&9));

    terminal.advance(&encode_minimap_drop(&MinimapDropCommand { content_id: 9 }));

    assert!(
        !terminal.minimap_contents.contains_key(&9),
        "drop retires the content store",
    );
}

#[test]
fn minimap_view_advance_reprojects_the_thumb() {
    let mut terminal = Terminal::new(4, 4, Theme::default());
    terminal.advance(&encode_minimap(&minimap_cmd(5, 9)));
    terminal.advance(&encode_minimap_view(&MinimapViewCommand {
        strip_id: 5,
        top_256: 256,
        visible_lines: 20,
    }));

    let mut grid = Grid::new(4, 4);
    terminal.project(&mut grid);
    assert_eq!(
        grid.minimaps()[0].view.as_ref().map(|v| v.top_256),
        Some(256),
        "the first projection stamps the initial thumb position",
    );

    // A view-only advance re-projects the strip so the thumb tracks the
    // scroll instead of freezing at its first position until a settle.
    terminal.advance(&encode_minimap_view(&MinimapViewCommand {
        strip_id: 5,
        top_256: 768,
        visible_lines: 20,
    }));
    terminal.project(&mut grid);
    assert_eq!(
        grid.minimaps()[0].view.as_ref().map(|v| v.top_256),
        Some(768),
        "the advanced view re-stamps the thumb at its new position",
    );
}

#[test]
fn minimap_projects_to_grid_joined_with_view_and_content() {
    let mut terminal = Terminal::new(4, 4, Theme::default());
    terminal.advance(&encode_minimap(&minimap_cmd(5, 9)));
    terminal.advance(&encode_minimap_view(&MinimapViewCommand {
        strip_id: 5,
        top_256: 512,
        visible_lines: 20,
    }));
    terminal.advance(&splice(
        9,
        0,
        0,
        vec![vec![run(0, 2, 1)], vec![run(1, 3, 2)]],
    ));

    let mut grid = Grid::new(4, 4);
    terminal.project(&mut grid);

    assert_eq!(grid.minimaps().len(), 1);
    let strip: &Minimap = &grid.minimaps()[0];
    assert_eq!(strip.strip.strip_id, 5);
    assert_eq!(
        strip.view,
        Some(MinimapView {
            top_256: 512,
            visible: 20,
        }),
    );
    assert_eq!(
        grid.minimap_content(9),
        summaries(vec![vec![run(0, 2, 1)], vec![run(1, 3, 2)]]),
    );
}

/// A line reaches the grid's store as the same allocation the terminal
/// holds, rather than a copy of it.
///
/// Every one of these hand-offs compares equal either way, so equality says
/// nothing about which happened. A file's worth of lines is copied per
/// hand-off if they are not shared, so the identity is the point.
#[test]
fn a_line_reaches_the_grid_without_being_copied() {
    let mut terminal = Terminal::new(4, 4, Theme::default());
    let mut grid = Grid::new(4, 4);

    terminal.advance(&splice(9, 0, 0, vec![vec![run(0, 2, 1)]]));
    terminal.project(&mut grid);

    let held = terminal.minimap_contents[&9][0].clone();
    assert!(
        Arc::ptr_eq(&grid.minimap_content(9)[0], &held),
        "the projected line is the terminal's line",
    );

    // A resize keeps the grid's store, so the line the grid holds is the one
    // it already had, which is the one the terminal holds.
    terminal.resize(8, 8);
    terminal.project(&mut grid);
    assert!(
        Arc::ptr_eq(&grid.minimap_content(9)[0], &held),
        "a resize leaves the grid holding the terminal's line",
    );
}

#[test]
fn projection_replays_splices_incrementally_into_the_grid() {
    let mut terminal = Terminal::new(4, 4, Theme::default());
    let mut grid = Grid::new(4, 4);

    terminal.advance(&splice(
        9,
        0,
        0,
        vec![vec![run(0, 2, 1)], vec![run(1, 3, 2)]],
    ));
    terminal.project(&mut grid);
    let epoch = grid.minimap_epoch();
    assert_eq!(
        grid.minimap_content(9),
        &terminal.minimap_contents[&9][..],
        "the first splice projects into the grid",
    );

    // A second splice replays against the grid store rather than re-cloning it.
    terminal.advance(&splice(9, 1, 1, vec![vec![run(2, 2, 4)]]));
    terminal.project(&mut grid);
    assert_eq!(
        grid.minimap_content(9),
        &terminal.minimap_contents[&9][..],
        "the replayed grid store equals the term store",
    );
    assert!(
        grid.minimap_epoch() > epoch,
        "a store change bumps the epoch"
    );
}

#[test]
fn projection_replays_a_drop_to_an_empty_grid_store() {
    let mut terminal = Terminal::new(4, 4, Theme::default());
    let mut grid = Grid::new(4, 4);

    terminal.advance(&splice(9, 0, 0, vec![vec![run(0, 2, 1)]]));
    terminal.project(&mut grid);
    assert_eq!(grid.minimap_content(9).len(), 1);

    terminal.advance(&encode_minimap_drop(&MinimapDropCommand { content_id: 9 }));
    terminal.project(&mut grid);
    assert!(
        grid.minimap_content(9).is_empty(),
        "a replayed drop empties the grid store",
    );
}

/// Counting entries alone leaves a writer sending whole-store splices
/// holding thousands of stores' worth of line handles, which is many times
/// the clone the journal exists to avoid. Counting handles alone leaves a
/// stream of drops unbounded, since a drop carries none.
#[test]
fn a_journal_gives_up_on_either_bound() {
    assert!(
        !journal_past_bounds(MAX_MINIMAP_JOURNAL, MAX_MINIMAP_JOURNAL_LINES),
        "a journal at both bounds is still the cheaper way"
    );
    assert!(
        journal_past_bounds(MAX_MINIMAP_JOURNAL + 1, 0),
        "a stream of drops is caught by the entry count alone"
    );
    assert!(
        journal_past_bounds(1, MAX_MINIMAP_JOURNAL_LINES + 1),
        "and one whole-store splice by the line total alone"
    );
}

/// Nothing drains the journal while projections are skipped, and they are
/// skipped for a scrolled-back view or an occluded window. Past the cap the
/// entries cost more than the clone they exist to avoid, so the projection
/// takes the clone and the journal resumes from there.
#[test]
fn an_over_cap_journal_reclones_once_and_then_replays_again() {
    let mut terminal = Terminal::new(4, 4, Theme::default());
    let mut grid = Grid::new(4, 4);

    // Seed a line so every splice below replaces rather than inserts, which
    // keeps the store one line long however many entries pile up.
    terminal.advance(&splice(9, 0, 0, vec![vec![run(0, 1, 1)]]));
    terminal.project(&mut grid);

    // One past the cap, with no projection between them to drain it.
    for class in 0..=MAX_MINIMAP_JOURNAL {
        terminal.advance(&splice(9, 0, 1, vec![vec![run(0, 2, class as u8)]]));
    }
    assert!(
        terminal.minimap_journal.is_empty() && terminal.minimap_reclone,
        "the journal gave up and asked for the whole map"
    );
    assert_eq!(
        terminal.minimap_journal_lines, 0,
        "and forgot what it was holding along with the entries"
    );

    // The clone reproduces whatever the stores hold when it runs, so an
    // entry recorded while the flag stands would be replayed on top of a
    // map that already has it.
    terminal.advance(&splice(9, 0, 1, vec![vec![run(1, 1, 5)]]));
    assert!(
        terminal.minimap_journal.is_empty(),
        "and records nothing more while it stands"
    );

    terminal.project(&mut grid);
    assert_eq!(
        grid.minimap_content(9),
        &terminal.minimap_contents[&9][..],
        "the clone carries what the dropped entries would have"
    );
    assert!(
        !terminal.minimap_reclone,
        "and the projection clears the flag"
    );

    // The journal is trusted again from here, so the next splice replays.
    terminal.advance(&splice(9, 0, 1, vec![vec![run(3, 3, 7)]]));
    assert_eq!(
        (
            terminal.minimap_journal.len(),
            terminal.minimap_journal_lines
        ),
        (1, 1),
        "the next change is journaled rather than dropped, lines and all"
    );
    terminal.project(&mut grid);
    assert_eq!(
        terminal.minimap_journal_lines, 0,
        "and the replay forgets its lines the way it forgets its entries"
    );
    assert_eq!(
        grid.minimap_content(9),
        summaries(vec![vec![run(3, 3, 7)]]),
        "and replaying it lands the change"
    );
}

/// A resize with nothing else pending leaves the grid's store where it was,
/// which is what makes the wholesale clone unnecessary.
#[test]
fn a_resize_leaves_the_grid_store_matching_the_terminal() {
    let mut terminal = Terminal::new(4, 4, Theme::default());
    let mut grid = Grid::new(4, 4);

    terminal.advance(&splice(
        9,
        0,
        0,
        vec![vec![run(0, 2, 1)], vec![run(1, 3, 2)]],
    ));
    terminal.project(&mut grid);

    terminal.resize(8, 8);
    terminal.project(&mut grid);
    assert_eq!(
        grid.minimap_content(9),
        &terminal.minimap_contents[&9][..],
        "the store the grid kept is still the terminal's",
    );
}

#[test]
fn a_projection_with_no_store_change_leaves_the_grid_untouched() {
    let mut terminal = Terminal::new(4, 4, Theme::default());
    let mut grid = Grid::new(4, 4);

    terminal.advance(&splice(9, 0, 0, vec![vec![run(0, 2, 1)]]));
    terminal.project(&mut grid);
    let epoch = grid.minimap_epoch();
    let store = grid.minimap_content(9).to_vec();

    // A viewport-only projection re-projects neither the store nor the epoch.
    terminal.project(&mut grid);
    assert_eq!(
        grid.minimap_content(9),
        &store[..],
        "the grid store is untouched"
    );
    assert_eq!(grid.minimap_epoch(), epoch, "the epoch is untouched");
}

/// A graphics frame rides the same APC stream as the stoatty commands, so
/// the terminal must route it to the image store and send the reply back
/// the way it answers any other query.
#[test]
fn a_graphics_frame_is_applied_and_answered() {
    use stoatty_protocol::kitty::{self, Action, ControlData, Format, ResponseResult};

    let mut terminal = Terminal::new(4, 8, Theme::default());

    // A 1x1 opaque pixel as raw RGBA, small enough to state inline.
    let mut frame = Vec::new();
    kitty::encode_into(
        &mut frame,
        &ControlData {
            action: Action::Transmit,
            format: Format::Rgba,
            width: 1,
            height: 1,
            id: 21,
            ..ControlData::default()
        },
        b"AQIDBA==",
    );
    terminal.advance(&frame);

    let response = kitty::parse_response(&terminal.take_responses())
        .expect("the terminal answers a frame that named an id");
    assert_eq!(
        (response.id, response.result),
        (21, ResponseResult::Ok),
        "the reply names the image it answers for",
    );
}

/// A page fill redirects the byte stream onto an off-screen context, but the
/// image store is persistent state a redirect must not swallow.
#[test]
fn a_graphics_frame_applies_during_a_page_fill() {
    use stoatty_protocol::kitty::{self, Action, ControlData, Format};

    let mut terminal = Terminal::new(4, 8, Theme::default());
    terminal.advance(&encode_pool_region(&PoolRegionCommand {
        pool: 1,
        window: 0,
        left: 0,
        top: 0,
        width: 8,
        height: 4,
        kind: PoolKind::Grid,
    }));
    terminal.advance(&encode_fill(&FillCommand { pool: 1, index: 0 }));

    let mut frame = Vec::new();
    kitty::encode_into(
        &mut frame,
        &ControlData {
            action: Action::Transmit,
            format: Format::Rgba,
            width: 1,
            height: 1,
            id: 22,
            ..ControlData::default()
        },
        b"AQIDBA==",
    );
    terminal.advance(&frame);

    assert!(
        kitty::parse_response(&terminal.take_responses()).is_some(),
        "the frame acted rather than being dropped with the page decorations",
    );
}

/// The renderer reads placements off the grid, so a placement that never
/// reaches the projection is one nothing draws.
#[test]
fn a_placement_reaches_the_grid_with_its_rect_and_pixels() {
    use stoatty_protocol::kitty::{self, Action, ControlData, Format};

    let mut terminal = Terminal::new(6, 20, Theme::default());
    let mut grid = Grid::new(6, 20);
    terminal.set_cell_pixels(8, 16);
    terminal.advance(b"\r\n\r\n  ");
    terminal.project(&mut grid);
    let epoch = grid.images_epoch();

    let mut out = Vec::new();
    kitty::encode_into(
        &mut out,
        &ControlData {
            action: Action::TransmitAndDisplay,
            format: Format::Rgba,
            width: 16,
            height: 16,
            id: 61,
            ..ControlData::default()
        },
        &base64_of(&[7u8; 16 * 16 * 4]),
    );
    terminal.advance(&out);
    terminal.project(&mut grid);

    let placed = grid.images();
    assert_eq!(placed.len(), 1, "the placement reaches the grid");
    assert_eq!(
        (
            placed[0].image,
            placed[0].row,
            placed[0].col,
            placed[0].cols,
            placed[0].rows,
        ),
        (61, 2, 2, 2, 1),
        "anchored at the cursor, sized 16x16 pixels over an 8x16 cell",
    );
    assert_eq!(
        (placed[0].width, placed[0].height, placed[0].rgba.len()),
        (16, 16, 16 * 16 * 4),
        "carrying the whole image rather than an id to look up",
    );
    assert!(
        grid.images_epoch() > epoch,
        "and moving the epoch, so a cached render pass rebuilds",
    );
}

/// Placements ride the text they were anchored beside, so output scrolling
/// the screen has to carry them along and drop them when they pass the top.
#[test]
fn scrolling_carries_a_placement_and_then_drops_it() {
    use stoatty_protocol::kitty::{self, Action, ControlData, Format};

    let mut terminal = Terminal::new(4, 20, Theme::default());
    let mut grid = Grid::new(4, 20);
    terminal.set_cell_pixels(8, 16);

    let mut out = Vec::new();
    kitty::encode_into(
        &mut out,
        &ControlData {
            action: Action::TransmitAndDisplay,
            format: Format::Rgba,
            width: 8,
            height: 16,
            id: 71,
            cursor_policy: 1,
            ..ControlData::default()
        },
        &base64_of(&[7u8; 8 * 16 * 4]),
    );
    terminal.advance(&out);
    terminal.project(&mut grid);
    assert_eq!(grid.images().first().map(|i| i.row), Some(0));

    // Two newlines past the last row, so the screen scrolls by two.
    terminal.advance(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");
    terminal.project(&mut grid);
    assert!(
        grid.images().is_empty(),
        "a placement scrolled off the top is gone, not pinned to row zero",
    );
}

/// The alternate screen is its own surface. A program that draws there and
/// leaves must not find its images still on the screen it returns to.
#[test]
fn the_alternate_screen_keeps_its_own_placements() {
    use stoatty_protocol::kitty::{self, Action, ControlData, Format};

    let mut terminal = Terminal::new(4, 20, Theme::default());
    let mut grid = Grid::new(4, 20);
    terminal.set_cell_pixels(8, 16);

    let place = |terminal: &mut Terminal, id| {
        let mut out = Vec::new();
        kitty::encode_into(
            &mut out,
            &ControlData {
                action: Action::TransmitAndDisplay,
                format: Format::Rgba,
                width: 8,
                height: 16,
                id,
                cursor_policy: 1,
                ..ControlData::default()
            },
            &base64_of(&[7u8; 8 * 16 * 4]),
        );
        terminal.advance(&out);
    };

    place(&mut terminal, 81);
    terminal.advance(b"\x1b[?1049h");
    place(&mut terminal, 82);
    terminal.project(&mut grid);
    assert_eq!(
        grid.images().iter().map(|i| i.image).collect::<Vec<_>>(),
        [82],
        "the alternate screen shows only what was placed on it",
    );

    terminal.advance(b"\x1b[?1049l");
    terminal.project(&mut grid);
    assert_eq!(
        grid.images().iter().map(|i| i.image).collect::<Vec<_>>(),
        [81],
        "and leaving it restores the primary screen's own placement",
    );

    terminal.advance(b"\x1b[?1049h");
    terminal.project(&mut grid);
    assert!(
        grid.images().is_empty(),
        "coming back finds a fresh alternate screen, not the last program's",
    );
}

/// A full reset returns the terminal to its start state, so the images a
/// client transmitted go with it. An erase does not: it clears the screen,
/// and the client places the same images again onto what it draws next.
#[test]
fn a_full_reset_drops_the_stored_images_where_an_erase_keeps_them() {
    use stoatty_protocol::kitty::{self, Action, ControlData, Format, ResponseResult};

    let mut terminal = Terminal::new(4, 20, Theme::default());
    let mut grid = Grid::new(4, 20);
    terminal.set_cell_pixels(8, 16);

    let mut transmit = Vec::new();
    kitty::encode_into(
        &mut transmit,
        &ControlData {
            action: Action::Transmit,
            format: Format::Rgba,
            width: 8,
            height: 16,
            id: 91,
            ..ControlData::default()
        },
        &base64_of(&[7u8; 8 * 16 * 4]),
    );
    terminal.advance(&transmit);
    terminal.take_responses();

    let mut put = Vec::new();
    kitty::encode_into(
        &mut put,
        &ControlData {
            action: Action::Put,
            id: 91,
            cursor_policy: 1,
            ..ControlData::default()
        },
        b"",
    );
    let place = |terminal: &mut Terminal, grid: &mut Grid| {
        terminal.advance(&put);
        terminal.project(grid);
        kitty::parse_response(&terminal.take_responses())
            .expect("a put naming an id is answered")
            .result
    };

    assert_eq!(place(&mut terminal, &mut grid), ResponseResult::Ok);
    assert_eq!(grid.images().len(), 1);

    terminal.advance(b"\x1b[2J");
    assert_eq!(
        place(&mut terminal, &mut grid),
        ResponseResult::Ok,
        "an erase leaves the image under its id, so a client can place it again",
    );

    terminal.advance(b"\x1bc");
    terminal.project(&mut grid);
    assert!(grid.images().is_empty(), "a reset takes the placements");
    assert!(
        matches!(
            place(&mut terminal, &mut grid),
            ResponseResult::Error { ref code, .. } if code == "ENOENT",
        ),
        "and the image itself, so the id names nothing afterward",
    );
}

/// A narrower grid leaves a placement anchored past the right edge with no
/// cell to sit on. Nothing scrolls columns, so unlike a row it cannot come
/// back, and moving it would put the image where the client never asked.
#[test]
fn a_resize_drops_a_placement_anchored_past_the_new_width() {
    use stoatty_protocol::kitty::{self, Action, ControlData, Format};

    let mut terminal = Terminal::new(6, 20, Theme::default());
    let mut grid = Grid::new(6, 20);
    terminal.set_cell_pixels(8, 16);
    // On the alternate screen, so a narrowing reflows nothing into history
    // and the resize is the only thing that can drop the placement.
    terminal.advance(b"\x1b[?1049h");
    terminal.advance(b"               ");

    let mut out = Vec::new();
    kitty::encode_into(
        &mut out,
        &ControlData {
            action: Action::TransmitAndDisplay,
            format: Format::Rgba,
            width: 8,
            height: 16,
            id: 95,
            cursor_policy: 1,
            ..ControlData::default()
        },
        &base64_of(&[7u8; 8 * 16 * 4]),
    );
    terminal.advance(&out);
    terminal.project(&mut grid);
    assert_eq!(
        grid.images().first().map(|image| image.col),
        Some(15),
        "anchored at the cursor, fifteen columns in",
    );

    terminal.resize(6, 8);
    terminal.project(&mut grid);
    assert!(
        grid.images().is_empty(),
        "eight columns leave nothing for a placement anchored at fifteen",
    );

    terminal.resize(6, 20);
    terminal.project(&mut grid);
    assert!(
        grid.images().is_empty(),
        "and widening again does not bring it back, since the resize dropped it",
    );
}

/// A 2x2 opaque PNG, built rather than pasted so what a test feeds and what
/// it asserts cannot drift apart.
fn png_2x2() -> Vec<u8> {
    let buffer = image::RgbaImage::from_pixel(2, 2, image::Rgba([9, 8, 7, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    buffer
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode png");
    out.into_inner()
}

/// One inline-image escape, with `args` before the payload.
fn iterm_escape(args: &str, image: &[u8]) -> Vec<u8> {
    format!("\x1b]1337;File={args}:{}\x1b\\", {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(image)
    })
    .into_bytes()
}

/// The payload is base64 text, which the parser would otherwise print as a
/// screenful of characters. Excising it is also what keeps the vte parser
/// from buffering a whole image.
#[test]
fn an_inline_image_payload_is_not_rendered_as_text() {
    let mut terminal = Terminal::new(4, 20, Theme::default());
    let mut grid = Grid::new(4, 20);
    terminal.set_cell_pixels(8, 16);

    // The cursor stays put, so the columns the text lands on are the ones the
    // payload would have taken had it reached the parser.
    let mut stream = iterm_escape("inline=1;doNotMoveCursor=1", &png_2x2());
    stream.extend_from_slice(b"hi");
    terminal.advance(&stream);
    terminal.project(&mut grid);

    assert_eq!(
        (grid.get(0, 0).ch, grid.get(0, 1).ch),
        ('h', 'i'),
        "only the text after the escape reaches the screen",
    );
    assert_eq!(*grid.get(0, 2), Cell::default(), "and nothing follows it");
}

#[test]
fn an_inline_image_lands_on_the_grid_at_the_cursor() {
    let mut terminal = Terminal::new(6, 20, Theme::default());
    let mut grid = Grid::new(6, 20);
    terminal.set_cell_pixels(8, 16);
    terminal.advance(b"\r\n  ");
    terminal.advance(&iterm_escape("inline=1;width=4;height=2", &png_2x2()));
    terminal.project(&mut grid);

    let placed = grid.images();
    assert_eq!(placed.len(), 1, "the escape places an image");
    assert_eq!(
        (placed[0].row, placed[0].col, placed[0].cols, placed[0].rows),
        (1, 2, 4, 2),
        "anchored at the cursor, in the cell box the client named",
    );
}

/// A client that asked the terminal not to move the cursor is drawing
/// around the image itself.
#[test]
fn the_cursor_moves_past_an_inline_image_unless_told_not_to() {
    let landed = |args: &str| {
        let mut terminal = Terminal::new(6, 20, Theme::default());
        let mut grid = Grid::new(6, 20);
        terminal.set_cell_pixels(8, 16);
        terminal.advance(&iterm_escape(args, &png_2x2()));
        let (cursor, _, _) = terminal.project(&mut grid);
        (cursor.row, cursor.col)
    };

    assert_eq!(
        landed("inline=1;width=4;height=2"),
        (1, 4),
        "the cursor lands on the image's last row and past its last column",
    );
    assert_eq!(
        landed("inline=1;width=4;height=2;doNotMoveCursor=1"),
        (0, 0),
        "and stays put when the client says so",
    );
}

/// inline=0 asks the terminal to offer the file as a download, which is a
/// file-manager feature rather than a rendering one.
#[test]
fn a_file_that_is_not_inline_draws_nothing() {
    let mut terminal = Terminal::new(4, 20, Theme::default());
    let mut grid = Grid::new(4, 20);
    terminal.set_cell_pixels(8, 16);

    terminal.advance(&iterm_escape("inline=0", &png_2x2()));
    terminal.project(&mut grid);

    assert!(grid.images().is_empty());
}

/// A client sends whatever format it has, and the escape names none, so the
/// decoder has only the bytes to go on.
#[test]
fn an_inline_image_decodes_whatever_format_it_arrives_in() {
    for format in [
        image::ImageFormat::Png,
        image::ImageFormat::Jpeg,
        image::ImageFormat::Gif,
        image::ImageFormat::Bmp,
    ] {
        let buffer = image::RgbaImage::from_pixel(2, 2, image::Rgba([9, 8, 7, 255]));
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(buffer)
            .into_rgb8()
            .write_to(&mut encoded, format)
            .expect("encode");

        let mut terminal = Terminal::new(4, 20, Theme::default());
        let mut grid = Grid::new(4, 20);
        terminal.set_cell_pixels(8, 16);
        terminal.advance(&iterm_escape("inline=1", &encoded.into_inner()));
        terminal.project(&mut grid);

        assert_eq!(grid.images().len(), 1, "{format:?} draws");
    }
}

/// A cell is taller than it is wide, so deriving one side from the other in
/// cell counts stretches every image by the cell's own aspect. The sizing
/// has to go through pixels.
#[test]
fn a_derived_dimension_keeps_the_image_shape_rather_than_the_cell_shape() {
    let sized = |args: &str, width: u32, height: u32| {
        let buffer = image::RgbaImage::from_pixel(width, height, image::Rgba([1, 2, 3, 255]));
        let mut encoded = std::io::Cursor::new(Vec::new());
        buffer
            .write_to(&mut encoded, image::ImageFormat::Png)
            .expect("encode");

        let mut terminal = Terminal::new(30, 60, Theme::default());
        let mut grid = Grid::new(30, 60);
        terminal.set_cell_pixels(8, 16);
        terminal.advance(&iterm_escape(args, &encoded.into_inner()));
        terminal.project(&mut grid);
        grid.images().first().map(|image| (image.cols, image.rows))
    };

    // A square image over an 8x16 cell: four columns are 32 pixels wide, so
    // matching that height takes two rows.
    assert_eq!(sized("inline=1;width=4", 64, 64), Some((4, 2)));
    assert_eq!(
        sized("inline=1;height=2", 64, 64),
        Some((4, 2)),
        "either side derives the other",
    );

    assert_eq!(
        sized("inline=1;width=4;height=8", 64, 64),
        Some((4, 2)),
        "a box is fitted inside rather than filled",
    );
    assert_eq!(
        sized("inline=1;width=4;height=8;preserveAspectRatio=0", 64, 64),
        Some((4, 8)),
        "unless the client asked for the box exactly",
    );
}

/// A percentage is of the screen, which is the only dimension a client
/// cannot know without asking.
#[test]
fn a_percent_dimension_is_of_the_screen() {
    let mut terminal = Terminal::new(30, 60, Theme::default());
    let mut grid = Grid::new(30, 60);
    terminal.set_cell_pixels(8, 16);
    terminal.advance(&iterm_escape(
        "inline=1;width=50%;height=10%;preserveAspectRatio=0",
        &png_2x2(),
    ));
    terminal.project(&mut grid);

    assert_eq!(
        grid.images().first().map(|image| (image.cols, image.rows)),
        Some((30, 3)),
        "half the columns and a tenth of the rows",
    );
}

/// An image wider than the terminal is one the client cannot see the rest
/// of, so the box stops at the screen.
#[test]
fn a_dimension_past_the_screen_is_capped_to_it() {
    let mut terminal = Terminal::new(6, 20, Theme::default());
    let mut grid = Grid::new(6, 20);
    terminal.set_cell_pixels(8, 16);
    terminal.advance(&iterm_escape(
        "inline=1;width=500;height=500;preserveAspectRatio=0",
        &png_2x2(),
    ));
    terminal.project(&mut grid);

    assert_eq!(
        grid.images().first().map(|image| (image.cols, image.rows)),
        Some((20, 6)),
    );
}

/// The terminal reserves two cells for a wide character, but nothing
/// downstream knew, so a bitmap rasterized for both landed in one.
#[test]
fn a_wide_character_marks_its_cell_and_blanks_the_spacer() {
    let (grid, _) = project(1, 8, "\u{1f600}ab".as_bytes());

    assert_eq!(
        (grid.get(0, 0).ch, grid.get(0, 1).ch),
        ('\u{1f600}', ' '),
        "the character sits in the first cell and the second is its spacer",
    );
    assert!(
        grid.get(0, 0).flags.contains(Flags::WIDE),
        "and the cell says it spans two",
    );
    assert!(
        !grid.get(0, 1).flags.contains(Flags::WIDE),
        "while the spacer does not claim a span of its own",
    );
    assert_eq!(
        (grid.get(0, 2).ch, grid.get(0, 3).ch),
        ('a', 'b'),
        "and what follows starts past both cells",
    );
    assert!(
        !grid.get(0, 2).flags.contains(Flags::WIDE),
        "a narrow character claims one cell",
    );
}
