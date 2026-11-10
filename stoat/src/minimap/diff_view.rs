//! The minimap strip of a per-file diff view, one row per display row.
//!
//! A diff view splices the base lines of a removal into its display, so its
//! strip summarizes the display rows the editor scrolls through rather than
//! the buffer's lines. A spliced base row reads the removed lane over its text
//! in the default class, and a live row takes its syntax colors and the lane
//! of its diff status. The rows follow the diff dials: the syntax toggle
//! colors them, and the tint level weights their change tint.
//!
//! Past one display row per strip row the rows fold into bands, as an editor's
//! strip folds its lines, read from the display rows themselves.

use super::{
    band::{self, BandSource, LineStat},
    density::{self, Density, Tier},
    diff_changes::{display_row, display_row_mark},
    emit::{convert_minimap_runs, minimap_line_tokens, strip_density, WEIGHTED_RUN_PROTOCOL},
    ClassTable, LineRuns, LineToken, RowMark, SYNC_BUDGET, SYNC_SLICE, TINT_WEIGHT,
};
use crate::{
    app::Stoat,
    display_map::{DisplayPoint, DisplaySnapshot},
    editor_state::EditorId,
    render::review::diff_tint_amount,
    workspace::{Workspace, WorkspaceId},
};
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
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
    /// Whether the rows take syntax colors.
    syntax_on: bool,
    /// The weight of the rows' change tint.
    tint_weight: u8,
    /// The [`ClassTable::class_version`] the rows were summarized under.
    classes: u64,
    /// How many display rows each row stands for.
    density: Density,
    /// A hash of the display, text, diff, and parse versions the rows show.
    versions: u64,
    /// The first row the pass in flight has not summarized yet, or `None` once
    /// the held rows show what the inputs ask for.
    next: Option<u32>,
}

impl DiffViewStrip {
    pub(crate) fn new(content_id: u32) -> Self {
        Self {
            content_id,
            budget: SYNC_BUDGET,
            held: Vec::new(),
            syntax_on: false,
            tint_weight: 0,
            classes: 0,
            density: Density::LINE,
            versions: 0,
            next: Some(0),
        }
    }

    /// The id naming the strip's content store on the terminal.
    pub(crate) fn content_id(&self) -> u32 {
        self.content_id
    }

    /// Whether the last sync left rows of a pass unsent.
    pub(crate) fn build_pending(&self) -> bool {
        self.next.is_some()
    }
}

/// A diff view's display rows, as a band's row reads them.
struct DisplayRows<'a> {
    snapshot: &'a DisplaySnapshot,
    /// The buffer rows that show, every one in a diff view.
    kept: &'a [Range<u32>],
    /// The syntax tokens of the buffer rows whose text a row summarizes.
    tokens: &'a HashMap<u32, Vec<LineToken>>,
    table: &'a ClassTable,
    comment_prefixes: &'a [&'a str],
}

impl BandSource for DisplayRows<'_> {
    fn stat(&self, row: u32) -> LineStat {
        band::line_stat(&self.snapshot.display_line(row), self.comment_prefixes)
    }

    fn mark(&self, row: u32) -> RowMark {
        display_row_mark(self.snapshot, row, self.kept, self.table)
    }

    fn content(&self, row: u32) -> LineRuns {
        display_row(self.snapshot, row, self.kept, self.tokens, self.table).0
    }
}

/// Bring the strip of diff view editor `editor_id` in workspace `ws_id` in step
/// with the editor's display rows, and append its splices to `out`.
///
/// A change of the syntax toggle, the tint level, the class table, the
/// density, the display, the text, the diff, or the parse starts a pass over
/// every row. Each sync summarizes the pass's rows in slices of at most
/// [`SYNC_SLICE`] until its time budget runs out, and sends the runs of rows
/// that differ from the held ones. The pass ends by cutting the held rows past
/// the display's last row.
///
/// The strip's density lands on the editor, which places the strip's thumb
/// and maps a press on the strip.
pub(crate) fn sync_diff_view_strip(
    stoat: &mut Stoat,
    ws_id: WorkspaceId,
    editor_id: EditorId,
    out: &mut Vec<u8>,
) {
    let weighted = stoat.stoatty_protocol >= WEIGHTED_RUN_PROTOCOL;
    let syntax_on = stoat.syntax_highlight && stoat.diff_syntax;
    let tint_weight = dial_tint_weight(stoat.diff_tint);
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
        buffers.syntax_version(buffer_id).hash(&mut hasher);
        buffers.lsp_token_version(buffer_id).hash(&mut hasher);
        hasher.finish()
    };
    let classes = minimap_class_table.class_version;
    let inputs = (syntax_on, tint_weight, classes, density, versions);
    if (
        strip.syntax_on,
        strip.tint_weight,
        strip.classes,
        strip.density,
        strip.versions,
    ) != inputs
    {
        (
            strip.syntax_on,
            strip.tint_weight,
            strip.classes,
            strip.density,
            strip.versions,
        ) = inputs;
        strip.next = Some(0);
    }
    let Some(mut row) = strip.next else {
        return;
    };

    let comment_prefixes = buffers
        .language_for(buffer_id)
        .map_or(&[][..], |language| language.line_comments);
    let buffer_rows = 0..snapshot.buffer_line_count();
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
        let tokens = match density.tier {
            Tier::Shape => HashMap::new(),
            Tier::Line | Tier::Fit => {
                let display = bands.start * density.lines_per_row
                    ..(bands.end * density.lines_per_row).min(display_rows);
                let hull = buffer_hull(&snapshot, display);
                minimap_line_tokens(&snapshot, buffer_id, syntax_on, minimap_class_table, hull)
            },
        };
        let source = DisplayRows {
            snapshot: &snapshot,
            kept: slice::from_ref(&buffer_rows),
            tokens: &tokens,
            table: minimap_class_table,
            comment_prefixes,
        };
        let lines: Vec<LineRuns> = bands
            .clone()
            .map(|at| {
                let band = density::band(at, density.lines_per_row, display_rows);
                band::band_row(
                    &source,
                    band,
                    density.tier,
                    minimap_class_table,
                    tint_weight,
                )
            })
            .collect();
        hold_rows(&mut strip.held, row, lines, &mut send);
        row = end;

        if row >= target {
            let excess = (strip.held.len() as u32).saturating_sub(target);
            if excess > 0 {
                send(target, excess, Vec::new());
                strip.held.truncate(target as usize);
            }
            strip.next = None;
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

/// The change tint's weight at diff tint level `level`, which the shipped
/// level draws at [`TINT_WEIGHT`] and level 0 draws at none.
fn dial_tint_weight(level: i8) -> u8 {
    let scale = diff_tint_amount(level) / diff_tint_amount(1);
    (f32::from(TINT_WEIGHT) * scale)
        .round()
        .min(f32::from(u8::MAX)) as u8
}

/// The buffer rows that the display rows `rows` of `snapshot` show, from the
/// first one's to the last one's.
///
/// Spliced base rows show no buffer row and widen nothing.
fn buffer_hull(snapshot: &DisplaySnapshot, rows: Range<u32>) -> Range<u32> {
    let buffer_row = |row: u32| {
        snapshot
            .display_to_buffer(DisplayPoint::new(row, 0))
            .map(|point| point.row)
    };
    match (
        rows.clone().find_map(buffer_row),
        rows.rev().find_map(buffer_row),
    ) {
        (Some(first), Some(last)) => first..last + 1,
        _ => 0..0,
    }
}

/// Hold `lines` as the rows from `start` on, and send each run of them that
/// differs from the held rows through `send`, as a start, a count of held rows
/// it replaces, and its rows.
///
/// A row past the held rows extends them. `start` is never past the held
/// rows' end, since a pass summarizes its rows in order from the first.
fn hold_rows(
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
        action_handlers::{focused_editor_mut, review::enter_diff_view},
        apc_emit,
        minimap::{emit, EdgeClass, CONTENT_COL, LANE_COLS},
        test_fixture::{drain_apc, mouse_event},
        test_harness::TestHarness,
    };
    use crossterm::event::{MouseButton, MouseEventKind};
    use std::path::Path;
    use stoat_config::MinimapMode;
    use stoatty_protocol::command::{Command, LineSummary};
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

        let mut h = TestHarness::with_size(120, 24);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        h.stoat.set_apc_tx(tx);
        h.stoat.stoatty_protocol = 13;
        h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
        h.stoat.set_diff_warm_auto(true);
        h.stage_review_scenario("/repo", &[("a.rs", &base.join("\n"), &live.join("\n"))]);
        h.open_file(Path::new("/repo/a.rs"));
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

    /// `store` with the splices of `commands` to content `content_id` applied.
    fn apply(store: &mut Vec<LineSummary>, content_id: u32, commands: &[Command]) {
        for command in commands {
            if let Command::MinimapLines(splice) = command
                && splice.content_id == content_id
            {
                let start = splice.start as usize;
                store.splice(start..start + splice.removed as usize, splice.lines.clone());
            }
        }
    }

    /// The class of `row`'s lane, `None` for a row with no lane.
    fn lane(row: &LineSummary) -> Option<u8> {
        row.first()
            .filter(|run| run.start_col == 0 && run.len == LANE_COLS as u8)
            .map(|run| run.class)
    }

    /// The weight of `row`'s change tint, the run of its lane's class from
    /// the content column on, `None` for a row with no tint.
    fn tint(row: &LineSummary) -> Option<u8> {
        let lane = lane(row)?;
        row.iter()
            .find(|run| run.start_col == CONTENT_COL as u8 && run.class == lane)
            .map(|run| run.weight)
    }

    /// The classes of `row`'s text, which follows its lane and its tint.
    fn text_classes(row: &LineSummary) -> Vec<u8> {
        let lane = lane(row);
        row.iter()
            .filter(|run| Some(run.class) != lane)
            .map(|run| run.class)
            .collect()
    }

    /// The strip holds a row per display row, 42 for 40 live lines and 2
    /// removed ones. The removed rows take the removed lane over text in the
    /// default class, and the added rows take the added lane.
    #[test]
    fn a_diff_view_strip_holds_a_row_per_display_row() {
        let (mut h, mut rx) = diff_view_harness();
        let mut store = Vec::new();
        let commands = frame(&mut h, &mut rx);
        apply(&mut store, strip_id(&h), &commands);

        let table = &h.stoat.minimap_class_table;
        let (removed, added) = (
            table.edge_class(EdgeClass::Removed),
            table.edge_class(EdgeClass::Added),
        );
        let expected: Vec<Option<u8>> = (0..42)
            .map(|row| match row {
                5 | 6 => Some(removed),
                21..=23 => Some(added),
                _ => None,
            })
            .collect();
        let removed_text: Vec<Vec<u8>> = [5, 6]
            .map(|row| {
                let mut classes = text_classes(&store[row]);
                classes.dedup();
                classes
            })
            .to_vec();

        assert_eq!(
            (store.iter().map(lane).collect::<Vec<_>>(), removed_text),
            (expected, vec![vec![0], vec![0]]),
        );
    }

    /// With the syntax toggle off, every row's text takes the default class,
    /// where the parse colored some of it before.
    #[test]
    fn the_syntax_toggle_sends_the_rows_in_the_default_class() {
        let (mut h, mut rx) = diff_view_harness();
        let mut store = Vec::new();
        let commands = frame(&mut h, &mut rx);
        let content_id = strip_id(&h);
        apply(&mut store, content_id, &commands);
        let colored =
            |store: &[LineSummary]| store.iter().flat_map(text_classes).any(|class| class != 0);
        let before = colored(&store);

        h.stoat.diff_syntax = false;
        let commands = frame(&mut h, &mut rx);
        apply(&mut store, content_id, &commands);

        assert_eq!((before, colored(&store), store.len()), (true, false, 42));
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

    /// A tint level of 0 sends the changed rows with no tint, the shipped
    /// level 1 sends the boards' 16 percent, and level 2 twice that.
    #[test]
    fn the_tint_level_weights_the_change_tint() {
        let (mut h, mut rx) = diff_view_harness();
        let mut store = Vec::new();
        let commands = frame(&mut h, &mut rx);
        let content_id = strip_id(&h);
        apply(&mut store, content_id, &commands);
        let mut tints = |h: &mut TestHarness, level: i8| {
            h.stoat.diff_tint = level;
            let commands = frame(h, &mut rx);
            apply(&mut store, content_id, &commands);
            [5, 21].map(|row| tint(&store[row]))
        };

        assert_eq!(
            [tints(&mut h, 0), tints(&mut h, 1), tints(&mut h, 2)],
            [[None, None], [Some(41), Some(41)], [Some(82), Some(82)]],
        );
    }
}
