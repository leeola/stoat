//! The minimap strip of a per-file diff view, one row per display row.
//!
//! A diff view splices the base lines of a removal into its display, so its
//! strip summarizes the display rows the editor scrolls through rather than
//! the buffer's lines. Every row draws its text in the diff text's gray, and
//! Change Marks color the parts that changed. An added part takes green on a
//! live row, and a removed base line takes red. The rows follow no diff dial.
//!
//! Past one display row per strip row the rows fold into bands, as an editor's
//! strip folds its lines, read from the display rows themselves. A display
//! that the Thumb Minimum's cap stops from fitting scrolls on the strip, with
//! a Coverage Rail beside it that marks each band of display rows holding a
//! hunk.
//!
//! A conflict view's strip follows its merged column the same way. Its rows
//! draw the merged text in the same gray, and its one Change Mark is the
//! conflict pink across each line of a chunk that still holds its markers.

use super::{
    band::{self, BandSource, LineStat},
    density::{self, Density, Tier},
    diff_changes::{conflict_changes, display_row, display_row_mark, row_changes},
    emit::{convert_minimap_runs, encode_rail_update, strip_density, WEIGHTED_RUN_PROTOCOL},
    rows::PassRail,
    LineRuns, RowMark, SYNC_BUDGET, SYNC_SLICE,
};
use crate::{
    app::Stoat,
    conflict_session,
    display_map::DisplaySnapshot,
    editor_state::EditorId,
    workspace::{Workspace, WorkspaceId},
};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    ops::Range,
    slice,
    time::Duration,
};
use stoat_config::MinimapMode;
use stoatty_protocol::command::{self, MinimapLinesCommand};

/// The minimap strip of one diff view editor, kept in step with the editor's
/// display rows.
///
/// The terminal holds the rows already sent, which `held` mirrors. A change of
/// what the rows show starts a pass over every row, and the pass sends only the
/// rows that come out different from the held ones, a slice at a time under
/// the sync budget.
pub(crate) struct DiffViewStrip {
    content_id: u32,
    /// Time one sync spends summarizing before it leaves the rest of a pass
    /// for the next frame.
    budget: Duration,
    /// The rows the terminal holds.
    held: Vec<LineRuns>,
    /// The [`ClassTable::class_version`] the rows were summarized under.
    classes: u64,
    /// How many display rows each row stands for.
    density: Density,
    /// A hash of the display, text, and diff versions the rows show.
    versions: u64,
    /// The first row the pass in flight has not summarized yet, or `None` once
    /// the held rows show what the inputs ask for.
    next: Option<u32>,
    /// The Coverage Rail the strip shows while the display does not fit it.
    rail: PassRail,
}

impl DiffViewStrip {
    /// A strip on content store `content_id`, whose Coverage Rail takes store
    /// `rail_id` when the display does not fit.
    pub(crate) fn new(content_id: u32, rail_id: u32) -> Self {
        Self {
            content_id,
            budget: SYNC_BUDGET,
            held: Vec::new(),
            classes: 0,
            density: Density::LINE,
            versions: 0,
            next: Some(0),
            rail: PassRail::new(rail_id),
        }
    }

    /// The id naming the strip's content store on the terminal.
    pub(crate) fn content_id(&self) -> u32 {
        self.content_id
    }

    /// The content id of the Coverage Rail the strip holds, `None` while the
    /// display fits the strip and so shows no rail.
    pub(crate) fn held_rail_id(&self) -> Option<u32> {
        self.rail.held_id()
    }

    /// Whether the last sync left rows of a pass unsent.
    pub(crate) fn build_pending(&self) -> bool {
        self.next.is_some()
    }
}

/// A slice of a diff view's display rows, as a band's row reads them.
///
/// The Change Marks of the slice's rows are read once, since the band rows and
/// the Coverage Rail both take them.
struct DisplayRows<'a> {
    snapshot: &'a DisplaySnapshot,
    /// The buffer rows that show, every one in a diff view.
    kept: &'a [Range<u32>],
    comment_prefixes: &'a [&'a str],
    /// The slice's first display row.
    first: u32,
    /// The Change Marks of each display row of the slice, from `first` on.
    changes: Vec<LineRuns>,
    /// The Coverage Rail's mark of each display row of the slice, from `first`
    /// on.
    marks: Vec<RowMark>,
}

impl BandSource for DisplayRows<'_> {
    fn stat(&self, row: u32) -> LineStat {
        band::line_stat(&self.snapshot.display_line(row), self.comment_prefixes)
    }

    fn content(&self, row: u32) -> LineRuns {
        display_row(self.snapshot, row, self.kept)
    }

    fn changes(&self, row: u32) -> LineRuns {
        self.changes[(row - self.first) as usize]
    }
}

/// Bring the strip of diff view editor `editor_id` in workspace `ws_id` in step
/// with the editor's display rows, and append its splices to `out`.
///
/// A conflict view editor's strip syncs here too. Its rows take the conflict
/// marks of its open chunks in place of a diff's Change Marks.
///
/// A change of the class table, the density, the display, the text, or the
/// diff starts a pass over every row. A parse or arrived LSP tokens start
/// none, since every row draws its text in one gray and reads no syntax token.
///
/// Each sync summarizes the pass's rows in slices of at most [`SYNC_SLICE`]
/// until its time budget runs out, and sends the runs of rows that differ from
/// the held ones. The pass ends by cutting the held rows past the display's
/// last row.
///
/// The strip's density lands on the editor, which places the strip's thumb
/// and maps a press on the strip.
///
/// While the display does not fit the strip, the pass also folds the marks of
/// its rows into the Coverage Rail, which goes out whole at the end of the
/// pass when it changed. A density that fits drops the rail as its pass
/// starts.
pub(crate) fn sync_diff_view_strip(
    stoat: &mut Stoat,
    ws_id: WorkspaceId,
    editor_id: EditorId,
    out: &mut Vec<u8>,
) {
    let weighted = stoat.stoatty_protocol >= WEIGHTED_RUN_PROTOCOL;
    let thumb_min_rows = stoat.minimap_thumb_min_rows();
    let single_strip = match stoat.minimap_mode() {
        MinimapMode::Single => Some(stoat.single_minimap_rect),
        _ => None,
    };
    let Stoat {
        workspaces,
        diff_view_strips,
        minimap_class_table,
        executor,
        ..
    } = stoat;
    let (Some(ws), Some(strip)) = (
        workspaces.get_mut(ws_id),
        diff_view_strips.get_mut(&(ws_id, editor_id)),
    ) else {
        return;
    };
    let Workspace {
        editors, buffers, ..
    } = ws;
    let Some(editor) = editors.get_mut(editor_id) else {
        return;
    };

    let buffer_id = editor.buffer_id;
    let snapshot = editor.display_map.snapshot();
    let display_rows = snapshot.line_count();
    let density = strip_density(
        display_rows,
        single_strip.unwrap_or(editor.minimap_rect),
        editor.viewport_rows.unwrap_or(0),
        thumb_min_rows,
    );
    editor.minimap_density = (density.tier != Tier::Line).then_some(density);
    let versions = {
        let mut hasher = DefaultHasher::new();
        snapshot.buffer_snapshot().version().hash(&mut hasher);
        snapshot.paint_version().hash(&mut hasher);
        editor.display_map.diff_version().hash(&mut hasher);
        hasher.finish()
    };
    let classes = minimap_class_table.class_version;
    let inputs = (classes, density, versions);
    if (strip.classes, strip.density, strip.versions) != inputs {
        (strip.classes, strip.density, strip.versions) = inputs;
        strip.next = Some(0);
    }
    let Some(mut row) = strip.next else {
        return;
    };
    if row == 0
        && let Some((rail_id, update)) = strip.rail.start(density, display_rows)
    {
        encode_rail_update(out, rail_id, update, weighted);
    }

    let comment_prefixes = buffers
        .language_for(buffer_id)
        .map_or(&[][..], |language| language.line_comments);
    let buffer_rows = 0..snapshot.buffer_line_count();
    let kept = slice::from_ref(&buffer_rows);
    let open = editor
        .conflict_view
        .as_deref()
        .map(|state| conflict_session::open_rows(state, &snapshot));
    let target = density::rows(display_rows, density.lines_per_row);
    let content_id = strip.content_id;
    let mut send = |start: u32, removed: u32, lines: Vec<LineRuns>| {
        let splice = MinimapLinesCommand {
            content_id,
            start,
            removed,
            lines: lines.into_iter().map(convert_minimap_runs).collect(),
        };
        match weighted {
            true => command::encode_minimap_runs_into(out, &splice),
            false => command::encode_minimap_lines_into(out, &splice),
        }
    };

    let started = executor.now();
    loop {
        let end = (row + SYNC_SLICE).min(target);
        let bands = row..end;
        let display = bands.start * density.lines_per_row
            ..(bands.end * density.lines_per_row).min(display_rows);
        let changes = match &open {
            Some(open) => {
                conflict_changes(&snapshot, display.clone(), kept, open, minimap_class_table)
            },
            None => row_changes(&snapshot, display.clone(), kept, minimap_class_table),
        };
        let marks = changes
            .iter()
            .map(|line| display_row_mark(line, minimap_class_table))
            .collect();
        let source = DisplayRows {
            snapshot: &snapshot,
            kept,
            comment_prefixes,
            first: display.start,
            changes,
            marks,
        };
        let lines: Vec<LineRuns> = bands
            .clone()
            .map(|at| {
                let band = density::band(at, density.lines_per_row, display_rows);
                band::change_band_row(&source, band, density.tier, minimap_class_table)
            })
            .collect();
        strip
            .rail
            .note(display.start, &source.marks, minimap_class_table);
        hold_rows(&mut strip.held, row, lines, &mut send);
        row = end;

        if row >= target {
            let excess = (strip.held.len() as u32).saturating_sub(target);
            if excess > 0 {
                send(target, excess, Vec::new());
                strip.held.truncate(target as usize);
            }
            strip.next = None;
            if let Some((rail_id, update)) = strip.rail.finish() {
                encode_rail_update(out, rail_id, update, weighted);
            }
            return;
        }
        // The clock is read after each slice, so a sync that arrives with the
        // budget spent still sends one.
        if executor.now().duration_since(started) >= strip.budget {
            strip.next = Some(row);
            return;
        }
    }
}

/// The viewport's top and span in display rows, for the thumb of a diff
/// view's strip and a press on it.
///
/// The strip holds one row per display row, so the view needs no conversion
/// to buffer lines. The span stops at the display's last row and is at least
/// 1.
pub(crate) fn diff_view_window(
    snapshot: &DisplaySnapshot,
    scroll_offset: f32,
    viewport_rows: u32,
) -> (f32, u16) {
    let top = scroll_offset.max(0.0);
    let left = snapshot.line_count().saturating_sub(top as u32);
    let visible = viewport_rows.min(left).max(1);
    (top, visible.min(u32::from(u16::MAX)) as u16)
}

/// Hold `lines` as the rows from `start` on, and send each run of them that
/// differs from the held rows through `send`, as a start, a count of held rows
/// it replaces, and its rows.
///
/// A row past the held rows extends them. `start` is never past the held
/// rows' end, since a pass summarizes its rows in order from the first.
pub(super) fn hold_rows(
    held: &mut Vec<LineRuns>,
    start: u32,
    lines: Vec<LineRuns>,
    send: &mut impl FnMut(u32, u32, Vec<LineRuns>),
) {
    let mut splice: Option<(u32, u32, Vec<LineRuns>)> = None;
    for (at, line) in (start..).zip(lines) {
        let index = at as usize;
        if held.get(index) == Some(&line) {
            if let Some((first, removed, lines)) = splice.take() {
                send(first, removed, lines);
            }
            continue;
        }

        let removed = u32::from(index < held.len());
        match held.get_mut(index) {
            Some(slot) => *slot = line,
            None => held.push(line),
        }
        match &mut splice {
            Some((_, count, lines)) => {
                *count += removed;
                lines.push(line);
            },
            None => splice = Some((at, removed, vec![line])),
        }
    }
    if let Some((first, removed, lines)) = splice {
        send(first, removed, lines);
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::{dispatch, focused_editor_mut, review::enter_diff_view},
        apc_emit,
        buffer_registry::RetainedLspTokens,
        display_map::highlights::BufferSemanticTokens,
        editor_state::EditorId,
        minimap::{density, emit, EdgeClass},
        mouse::MinimapDrag,
        render::rail_strip_id,
        test_fixture::{
            apply_minimap, drain_apc, marked_rows, minimap_declarations, mouse_event, rail_mark,
        },
        test_harness::TestHarness,
    };
    use crossterm::event::{MouseButton, MouseEventKind};
    use ratatui::layout::Rect;
    use std::{path::Path, sync::Arc, time::Duration};
    use stoat_action::{Conflict, ConflictPickOurs, DiffSyntax, DiffTintUp};
    use stoat_config::MinimapMode;
    use stoatty_protocol::{
        command::{Command, LineSummary},
        PROTOCOL_VERSION,
    };
    use tokio::sync::mpsc::UnboundedReceiver;

    /// A diff view of a 40-line Rust file whose live text drops base lines 5
    /// and 6 and adds three lines after base line 20, parsed, on a stoatty of
    /// protocol 13 at 120 by 24 with the per-pane strip.
    ///
    /// The display holds the 40 live rows and the 2 removed base rows, which
    /// splice in at display rows 5 and 6. The added lines take display rows 21
    /// to 23.
    fn diff_view_harness() -> (TestHarness, UnboundedReceiver<Vec<u8>>) {
        let line = |n: usize| format!("let x{n} = {n};");
        let base: Vec<String> = (0..39).map(line).collect();
        let mut live: Vec<String> = base.clone();
        live.drain(5..7);
        live.splice(
            19..19,
            ["let a = 1;", "let b = 2;", "let c = 3;"].map(String::from),
        );
        diff_view_of("a.rs", &base, &live)
    }

    /// A diff view of a 600-line file whose live text drops base lines 100
    /// and 101 and adds three lines after base line 399, on a stoatty of the
    /// current protocol at 120 by 25 with the per-pane strip. Its status band
    /// takes two rows, so the pane keeps 23.
    ///
    /// The display holds 603 rows, the removed base rows at display rows 100
    /// and 101 and the added lines at display rows 400 to 402. They want 5
    /// rows a row on the 138-row strip, inside the default Thumb Minimum's
    /// cap of 11, so the strip fits.
    fn long_diff_view() -> (TestHarness, UnboundedReceiver<Vec<u8>>) {
        let base: Vec<String> = (0..600).map(|n| format!("line {n}")).collect();
        let mut live = base.clone();
        live.splice(
            400..400,
            ["added a", "added b", "added c"].map(String::from),
        );
        live.drain(100..102);
        let (mut h, rx) = diff_view_of("long.txt", &base, &live);
        h.stoat.stoatty_protocol = PROTOCOL_VERSION;
        h.resize(120, 25);
        (h, rx)
    }

    /// The long diff view under a Thumb Minimum of 5 rows, whose cap of 4
    /// rows a row stops the display from fitting. The strip scrolls beside a
    /// Coverage Rail of 121 rows at 5 display rows each.
    fn railed_diff_view() -> (TestHarness, UnboundedReceiver<Vec<u8>>) {
        let (mut h, rx) = long_diff_view();
        h.stoat.settings.editor_minimap_thumb_min_rows = Some(5);
        (h, rx)
    }

    /// The focused diff view of /repo/`file` from `base` to `live`, parsed,
    /// on a stoatty of protocol 13 at 120 by 24 with the per-pane strip.
    fn diff_view_of(
        file: &str,
        base: &[String],
        live: &[String],
    ) -> (TestHarness, UnboundedReceiver<Vec<u8>>) {
        let mut h = TestHarness::with_size(120, 24);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        h.stoat.set_apc_tx(tx);
        h.stoat.stoatty_protocol = 13;
        h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
        h.stoat.set_diff_warm_auto(true);
        h.stage_review_scenario("/repo", &[(file, &base.join("\n"), &live.join("\n"))]);
        h.open_file(Path::new(&format!("/repo/{file}")));
        h.settle_diff_jobs();
        enter_diff_view(&mut h.stoat);
        h.settle();
        for _ in 0..2 {
            h.stoat.drive_background();
            h.settle();
        }
        (h, rx)
    }

    /// Paint a frame, emit its scene and its minimap frames, and decode what
    /// went out.
    fn frame(h: &mut TestHarness, rx: &mut UnboundedReceiver<Vec<u8>>) -> Vec<Command> {
        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        emit::emit_minimap(&mut h.stoat);
        drain_apc(rx)
    }

    /// The id of the focused diff view editor's strip.
    fn strip_id(h: &TestHarness) -> u32 {
        let editor_id = h.stoat.focused_editor_ids().expect("an editor").0;
        h.stoat.diff_view_strips[&(h.stoat.active_workspace, editor_id)].content_id()
    }

    /// The classes of `row`'s runs in order, with each repeat of the class
    /// before it dropped, so a row reads its text's class and then its marks'.
    fn classes(row: &LineSummary) -> Vec<u8> {
        let mut classes: Vec<u8> = row.iter().map(|run| run.class).collect();
        classes.dedup();
        classes
    }

    /// The strip holds a row per display row, 42 for 40 live lines and 2
    /// removed ones. Every row draws its text in the diff gray. The removed
    /// rows take a removed mark and the added rows an added mark, and the rows
    /// that hold take none.
    #[test]
    fn a_diff_view_strip_holds_a_row_per_display_row() {
        let (mut h, mut rx) = diff_view_harness();
        let mut store = Vec::new();
        let commands = frame(&mut h, &mut rx);
        apply_minimap(&mut store, strip_id(&h), &commands);

        let table = &h.stoat.minimap_class_table;
        let class = |kind| table.edge_class(kind);
        let expected: Vec<Vec<u8>> = (0..42)
            .map(|row| match row {
                5 | 6 => vec![class(EdgeClass::DiffText), class(EdgeClass::MarkRemoved)],
                21..=23 => vec![class(EdgeClass::DiffText), class(EdgeClass::MarkAdded)],
                _ => vec![class(EdgeClass::DiffText)],
            })
            .collect();
        assert_eq!(store.iter().map(classes).collect::<Vec<_>>(), expected);
    }

    /// A conflict view's strip holds a row per display row of its merged
    /// column, each in the diff gray, and the pane declares that strip. The
    /// rows of both open chunks, merged rows 1 to 5 and 7 to 11, end in a
    /// conflict mark. A pick of ours settles the first chunk to its one line,
    /// so the second chunk's rows, then 3 to 7, keep the only marks.
    ///
    /// The merged text ends in a newline, so its last display row is empty and
    /// holds no run.
    #[test]
    fn a_conflict_view_strip_marks_its_open_chunk_alone() {
        let mut h = TestHarness::with_size(120, 24);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        h.stoat.set_apc_tx(tx);
        h.stoat.stoatty_protocol = 13;
        h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root).conflicted_file(
            "f.txt",
            Some("a\nb\nc\nd\ne\n"),
            Some("a\nB\nc\nD\ne\n"),
            Some("a\nX\nc\nY\ne\n"),
        );
        dispatch(&mut h.stoat, &Conflict);
        h.settle();
        let mut store = Vec::new();
        let commands = frame(&mut h, &mut rx);
        apply_minimap(&mut store, strip_id(&h), &commands);
        let opened: Vec<Vec<u8>> = store.iter().map(classes).collect();
        let (_, pane, strip, _) = focused_strip(&mut h);
        let declared = (
            minimap_declarations(&commands),
            vec![(pane, strip.x, strip.width, strip_id(&h))],
        );

        dispatch(&mut h.stoat, &ConflictPickOurs);
        h.settle();
        let commands = frame(&mut h, &mut rx);
        apply_minimap(&mut store, strip_id(&h), &commands);

        let table = &h.stoat.minimap_class_table;
        let (text, conflict) = (
            table.edge_class(EdgeClass::DiffText),
            table.edge_class(EdgeClass::MarkConflict),
        );
        let rows = |lines: u32, marked: &[(u32, u32)]| -> Vec<Vec<u8>> {
            (0..lines)
                .map(|row| {
                    match marked
                        .iter()
                        .any(|&(start, end)| (start..end).contains(&row))
                    {
                        true => vec![text, conflict],
                        false => vec![text],
                    }
                })
                .chain([Vec::new()])
                .collect()
        };
        assert_eq!(
            declared.0, declared.1,
            "the pane declares the conflict strip"
        );
        assert_eq!(
            (opened, store.iter().map(classes).collect::<Vec<_>>()),
            (rows(13, &[(1, 6), (7, 12)]), rows(9, &[(3, 8)])),
        );
    }

    /// A step of the tint dial or of the syntax toggle sends no row, since the
    /// strip draws its text in the diff gray and its marks at full weight at
    /// every setting of the two.
    #[test]
    fn a_dial_step_sends_no_row() {
        let (mut h, mut rx) = diff_view_harness();
        frame(&mut h, &mut rx);
        let content_id = strip_id(&h);

        dispatch(&mut h.stoat, &DiffTintUp);
        dispatch(&mut h.stoat, &DiffSyntax);
        let sent = frame(&mut h, &mut rx)
            .into_iter()
            .filter(|command| match command {
                Command::MinimapLines(splice) => splice.content_id == content_id,
                Command::MinimapLen(len) => len.content_id == content_id,
                _ => false,
            })
            .count();
        assert_eq!(
            (h.stoat.diff_tint, h.stoat.diff_syntax, sent),
            (2, false, 0)
        );
    }

    /// A parse and arrived LSP tokens start no pass over the strip's rows,
    /// since every row draws its text in one gray, and the parse moves no part
    /// of the display's paint version. An edit starts a pass.
    ///
    /// A Thumb Minimum past the viewport's rows caps a strip row at one display
    /// row, so the 600 rows take two slices, and a pass that starts at a zero
    /// budget leaves rows unsent after its frame.
    #[test]
    fn a_parse_or_lsp_tokens_start_no_pass_over_the_strip() {
        let base: Vec<String> = (0..600).map(|n| format!("let x{n} = {n};")).collect();
        let mut live = base.clone();
        live[300] = "let changed = 1;".to_string();
        let (mut h, mut rx) = diff_view_of("long.rs", &base, &live);
        h.stoat.settings.editor_minimap_thumb_min_rows = Some(100);
        let (editor_id, buffer_id) = h.stoat.focused_editor_ids().expect("an editor");
        let key = (h.stoat.active_workspace, editor_id);
        for _ in 0..4 {
            frame(&mut h, &mut rx);
        }
        let settled = !h.stoat.diff_view_strips[&key].build_pending();
        h.stoat
            .diff_view_strips
            .get_mut(&key)
            .expect("the strip")
            .budget = Duration::ZERO;
        let pending = |h: &mut TestHarness, rx: &mut UnboundedReceiver<Vec<u8>>| {
            frame(h, rx);
            h.stoat.diff_view_strips[&key].build_pending()
        };
        let paint = |h: &mut TestHarness| {
            focused_editor_mut(&mut h.stoat)
                .expect("an editor")
                .display_map
                .snapshot()
                .paint_version()
        };
        let painted = paint(&mut h);
        let parsed = h
            .stoat
            .active_workspace()
            .buffers
            .syntax_version(buffer_id)
            .is_some();

        h.stoat
            .active_workspace_mut()
            .buffers
            .clear_syntax(buffer_id);
        let after_parse = (pending(&mut h, &mut rx), paint(&mut h) == painted);
        let interner = h.stoat.syntax_styles.interner.clone();
        let channel = BufferSemanticTokens::new(Arc::from(Vec::new()), interner, |_| 0);
        h.stoat.active_workspace_mut().buffers.store_lsp_tokens(
            buffer_id,
            RetainedLspTokens {
                version: 0,
                channel,
            },
        );
        let after_tokens = pending(&mut h, &mut rx);
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("buffer poisoned")
            .edit(0..0, "x");
        let after_edit = pending(&mut h, &mut rx);

        assert_eq!(
            (settled, parsed, after_parse, after_tokens, after_edit),
            (true, true, (false, true), false, true),
        );
    }

    /// The thumb counts display rows, so a view whose top is display row 10
    /// ships top 10. The same top is buffer line 8, since two removed rows lie
    /// above it.
    #[test]
    fn the_thumb_counts_display_rows() {
        let (mut h, mut rx) = diff_view_harness();
        frame(&mut h, &mut rx);
        {
            let editor = focused_editor_mut(&mut h.stoat).expect("an editor");
            editor.scroll_row = 10;
            editor.scroll_offset = 10.0;
        }
        let _ = h.stoat.render();
        apc_emit::emit_smooth_scroll(&mut h.stoat);
        let top = drain_apc(&mut rx)
            .iter()
            .rev()
            .find_map(|command| match command {
                Command::MinimapView(view) => Some(view.top_256),
                _ => None,
            });

        assert_eq!(top, Some(10 * 256));
    }

    /// A press on the strip lands on the display row under it. The fifth
    /// cell's middle is row 27, which the view centers. Buffer line 27 sits at
    /// display row 29, two rows lower.
    #[test]
    fn a_strip_press_lands_on_a_display_row() {
        let (mut h, mut rx) = diff_view_harness();
        frame(&mut h, &mut rx);
        let (strip, viewport) = {
            let editor = focused_editor_mut(&mut h.stoat).expect("an editor");
            let strip = editor.minimap_rect.expect("the paint reserved a strip");
            (strip, editor.viewport_rows.expect("a laid out viewport"))
        };
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            strip.x + 3,
            strip.y + 4,
        ));
        let scroll_row = focused_editor_mut(&mut h.stoat)
            .expect("an editor")
            .scroll_row;

        assert_eq!(scroll_row, 27 - viewport / 2);
    }

    /// The focused diff view's id, its pane's index, its strip rect, and its
    /// viewport rows.
    fn focused_strip(h: &mut TestHarness) -> (EditorId, u32, Rect, u32) {
        let ws = h.stoat.active_workspace();
        let pane = ws.panes.pane(ws.panes.focus()).index;
        let editor = focused_editor_mut(&mut h.stoat).expect("an editor");
        let strip = editor.minimap_rect.expect("the paint reserved a strip");
        let viewport = editor.viewport_rows.expect("a laid out viewport");
        let editor_id = h.stoat.focused_editor_ids().expect("an editor").0;
        (editor_id, pane, strip, viewport)
    }

    /// A display that stops fitting its strip declares a Coverage Rail in the
    /// strip's last cell on the frame after the rail's pass, though nothing
    /// else on the pane moved. The rail holds the 603 display rows at 5 a
    /// row, marks the removed rows on row 20 and the added rows on row 80, and
    /// its thumb covers the 138 rows of 4 display rows the strip shows.
    #[test]
    fn a_display_past_the_cap_declares_a_rail_over_its_rows() {
        let (mut h, mut rx) = long_diff_view();
        frame(&mut h, &mut rx);
        frame(&mut h, &mut rx);
        h.stoat.settings.editor_minimap_thumb_min_rows = Some(5);
        let first = frame(&mut h, &mut rx);
        let second = frame(&mut h, &mut rx);
        apc_emit::emit_smooth_scroll(&mut h.stoat);
        let views = drain_apc(&mut rx);

        let (_, pane, strip, _) = focused_strip(&mut h);
        let content_id = strip_id(&h);
        let mut rail = Vec::new();
        apply_minimap(&mut rail, content_id + 1, &first);
        let rail_view = views.iter().find_map(|command| match command {
            Command::MinimapView(view) if view.strip_id == rail_strip_id(pane) => {
                Some((view.top_256, view.visible_256))
            },
            _ => None,
        });
        let table = &h.stoat.minimap_class_table;
        let (removed, added) = (
            table.edge_class(EdgeClass::Removed),
            table.edge_class(EdgeClass::Added),
        );

        assert_eq!(
            (
                minimap_declarations(&second),
                rail.len(),
                marked_rows(&rail),
                rail_view
            ),
            (
                vec![
                    (pane, strip.x, strip.width - 1, content_id),
                    (
                        rail_strip_id(pane),
                        strip.x + strip.width - 1,
                        1,
                        content_id + 1
                    ),
                ],
                121,
                vec![(20, rail_mark(removed)), (80, rail_mark(added))],
                Some((0, 138 * 4 * 256 / 5)),
            ),
            "the strip and the rail declared, the rail's rows, its marks, and \
             its view",
        );
    }

    /// A press on the rail lands on the display row the rail shows under it,
    /// which the view centers. Cell 15's middle is rail row 93, whose band of
    /// 5 display rows centers on display row 467. Buffer line 467 sits two
    /// rows lower, under the removed rows.
    #[test]
    fn a_rail_press_lands_on_its_display_row() {
        let (mut h, mut rx) = railed_diff_view();
        frame(&mut h, &mut rx);
        let (editor_id, _, strip, viewport) = focused_strip(&mut h);
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            strip.x + strip.width - 1,
            strip.y + 15,
        ));
        let scroll_row = focused_editor_mut(&mut h.stoat)
            .expect("an editor")
            .scroll_row;

        assert_eq!(
            (scroll_row, h.stoat.minimap_drag),
            (467 - viewport / 2, Some(MinimapDrag::Rail(editor_id))),
        );
    }

    /// The rail's thumb counts display rows, as the strip's does. A view whose
    /// top is display row 450 is buffer line 448 of 601, and a thumb over
    /// those buffer lines sits higher.
    #[test]
    fn the_rail_thumb_counts_display_rows() {
        let (mut h, mut rx) = railed_diff_view();
        frame(&mut h, &mut rx);
        let (_, pane, strip, viewport) = focused_strip(&mut h);
        let density = {
            let editor = focused_editor_mut(&mut h.stoat).expect("an editor");
            editor.scroll_row = 450;
            editor.scroll_offset = 450.0;
            editor.minimap_density.expect("a folded strip")
        };
        let _ = h.stoat.render();
        apc_emit::emit_smooth_scroll(&mut h.stoat);
        let view = drain_apc(&mut rx)
            .iter()
            .rev()
            .find_map(|command| match command {
                Command::MinimapView(view) if view.strip_id == rail_strip_id(pane) => {
                    Some((view.top_256, view.visible_256))
                },
                _ => None,
            });
        let strip_rows = u32::from(strip.height) * 6;
        let (top, visible) = density::rail_window(450.0, viewport, 603, density, strip_rows, 5);

        assert_eq!(view, Some(((top * 256.0) as u32, visible)));
    }

    /// Leaving the diff view drops the strip's store and its rail's.
    #[test]
    fn leaving_the_diff_view_drops_its_rail() {
        let (mut h, mut rx) = railed_diff_view();
        frame(&mut h, &mut rx);
        let content_id = strip_id(&h);

        h.stoat.toggle_diff_view();
        h.settle();
        let drops: Vec<u32> = frame(&mut h, &mut rx)
            .iter()
            .filter_map(|command| match command {
                Command::MinimapDrop(drop) => Some(drop.content_id),
                _ => None,
            })
            .collect();

        assert_eq!(drops, [content_id, content_id + 1]);
    }
}
