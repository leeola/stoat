//! Pure per-line run summaries feeding the stoatty minimap strip.
//!
//! A [`MinimapContent`] holds one summary per buffer line, updated incrementally
//! from buffer edits and filled progressively for a freshly opened file, and
//! drains the changes as [`Splice`]s the emission layer turns into `minimap_lines`
//! frames. [`summarize_line`] compresses a line into colored [`Run`] blocks, never
//! the file text. Nothing here does IO, so the whole module is unit-tested.

use crate::{
    display_map::{highlights::HighlightStyleId, syntax_theme::SyntaxStyles},
    host::DiffStatus,
    theme::{scope, Theme},
};
use ratatui::style::Color;
use std::{collections::HashMap, ops::Range, time::Duration};
use stoat_language::HighlightId;
use stoat_scheduler::Executor;
use stoat_text::{
    patch::{Edit, Patch},
    Rope,
};

pub(crate) mod diff_changes;
pub(crate) mod emit;

/// Minimap columns a line is summarized into, matching the strip's declared
/// `max_columns`. Display columns past this are dropped.
const MAX_COLUMNS: u32 = 120;

/// Display width of a tab stop.
const TAB_WIDTH: u32 = 4;

/// Runs kept per line. The last run swallows any overflow to end-of-line, so a
/// busy line never emits an unbounded number of blocks.
///
/// A line splits on glyph weight as well as on class, so a line of code carries
/// roughly twice the runs its coloring alone would give.
const MAX_RUNS: usize = 24;

/// Time [`MinimapContent::sync`] spends on chunked work before it leaves the
/// rest for the next frame.
///
/// The build and both sweeps share one budget, since what a frame has to spare
/// belongs to the frame rather than to the stage.
///
/// A row's cost follows its content. A plain line summarizes in well under a
/// microsecond. A long highlighted line resolves a token set and allocates a
/// run vector. A fixed row count therefore bounds the frame only for the file
/// it was tuned on.
const SYNC_BUDGET: Duration = Duration::from_millis(6);

/// Rows one stage of [`MinimapContent::sync`] covers before it reads the clock
/// again.
///
/// The count is small enough that the slowest rows overrun [`SYNC_BUDGET`] by a
/// fraction of a frame. It is large enough that a file of plain lines does not
/// pay a clock read every few rows.
const SYNC_SLICE: u32 = 512;

/// Changed marks in one window past which they are applied as a batch.
///
/// Applying one at a time shifts the entries after it, so a window's cost is
/// quadratic in how many marks moved. Rebuilding the window's slice instead is
/// linear but allocates, which is not worth it for the handful a diagnostic
/// batch usually moves.
const EDGE_BATCH_MIN: usize = 8;

/// Edits one [`MinimapContent::sync`] re-summarizes on its own frame.
///
/// A patch with more edits splices the store in one pass and hands its rows to
/// the recolor sweep, so the frame pays [`SYNC_BUDGET`] rather than a token
/// query per edit. A pasted or indented block of a few dozen lines stays exact
/// on its own frame.
const EDIT_SUMMARIZE_MAX: usize = 32;

/// Line count past which a buffer disables its minimap, so a huge file neither
/// summarizes nor emits.
const MAX_LINES: usize = 500_000;

/// Buffer lines drawn per vertical strip cell, matching the strip's declared
/// `lines_per_cell`. A pointer row therefore spans this many lines.
pub const LINES_PER_CELL: u8 = 8;

/// Columns reserved on a line's left edge for the diff/diagnostic mark lane, so
/// a mark never overwrites the code silhouette. Content starts after it.
const LANE_WIDTH: u32 = 2;

/// A single colored run on one line, `len` display columns from `start_col`
/// drawn in palette class `class` at opacity `weight`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Run {
    pub start_col: u8,
    pub len: u8,
    pub class: u8,
    /// How much ink the glyphs under this run put on the page, from
    /// [`glyph_weight`]. A run covers one weight class, so a word and the
    /// punctuation beside it are separate runs even in one color.
    pub weight: u8,
}

/// One line's runs, held inline rather than in a heap allocation of their own.
///
/// A [`Run`] is four bytes and [`MAX_RUNS`] caps a line at twenty-four of them,
/// so the whole summary is ninety-seven bytes held inline. One allocation per
/// buffer line is what a hundred-thousand-line file paid to build its strip,
/// and every splice paid again to carry a copy to the wire.
///
/// Copies rather than clones for the same reason, so a splice that hands the
/// emitter its payload is a memcpy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LineRuns {
    runs: [Run; MAX_RUNS],
    len: u8,
}

impl Default for LineRuns {
    fn default() -> Self {
        Self {
            runs: [Run {
                start_col: 0,
                len: 0,
                class: 0,
                weight: 0,
            }; MAX_RUNS],
            len: 0,
        }
    }
}

impl LineRuns {
    pub fn as_slice(&self) -> &[Run] {
        &self.runs[..self.len as usize]
    }

    /// Append `run`, or drop it once the line holds [`MAX_RUNS`].
    ///
    /// Dropping is what the summary wants rather than a failure to report.
    /// [`summarize_line`] widens its last run to swallow the rest of the line
    /// past the cap, so a caller reaching this has already decided the run has
    /// nowhere to go.
    fn push(&mut self, run: Run) {
        if (self.len as usize) < MAX_RUNS {
            self.runs[self.len as usize] = run;
            self.len += 1;
        }
    }

    fn last(&self) -> Option<&Run> {
        self.as_slice().last()
    }

    fn last_mut(&mut self) -> Option<&mut Run> {
        self.runs[..self.len as usize].last_mut()
    }

    fn len(&self) -> usize {
        self.len as usize
    }
}

/// A line's diff or diagnostic state, drawn as a colored run in the reserved
/// left-edge lane.
///
/// The seven kinds occupy palette classes appended after the syntax scopes,
/// resolved to a class via [`ClassTable::edge_class`]. `Removed` is reserved for
/// palette stability but not sourced per-line yet, since a deleted line has no
/// buffer row to mark.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EdgeClass {
    Added,
    Removed,
    Modified,
    Error,
    Warning,
    Info,
    /// A chunk header row, in the color the commit preview and the changes view
    /// paint chunk headers in.
    Header,
}

/// The edge-lane mark of a row, and the offset every content class on the row
/// takes.
///
/// The offset is how a row's diff status reaches the palette. The class table
/// lays out one copy of the syntax classes per status, so a token's class plus
/// the row's offset names the token's color in that status's block.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RowMark {
    /// The edge-lane class, or `None` for a row with no lane mark.
    pub edge: Option<u8>,
    /// The offset every content class on the row takes, 0 for the plain syntax
    /// block.
    pub class_base: u8,
}

impl RowMark {
    /// A row with no lane mark, whose content takes the plain syntax classes.
    pub const NONE: RowMark = RowMark {
        edge: None,
        class_base: 0,
    };
}

/// A token's byte range within a line, already resolved to its palette class.
///
/// The style-to-class resolution lives in [`ClassTable`]; [`summarize_line`] takes
/// the resolved class so it stays free of the theme.
#[derive(Clone, Debug)]
pub struct LineToken {
    pub range: Range<usize>,
    pub class: u8,
}

/// A pending change to a content store, replacing `removed` lines from `start`
/// with [`Self::lines`]. The inserted count is `lines.len()`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Splice {
    pub start: u32,
    pub removed: u32,
    pub lines: Vec<LineRuns>,
}

/// The decoration and syntax versions a [`MinimapContent::sync`] re-checks its
/// built lines against, distinct from the buffer edit version.
///
/// Either changing without a buffer edit re-summarizes the affected built lines.
#[derive(Clone, Copy)]
pub struct SyncVersions {
    /// Combined diff and diagnostic version. A change re-checks the edge marks.
    pub decoration: u64,
    /// Combined highlight-toggle and parse version. A change re-summarizes the
    /// content runs.
    pub syntax: u64,
    /// The part of [`Self::syntax`] contributed by inputs other than the
    /// tree-sitter parse, currently the highlight toggle and the LSP tokens.
    ///
    /// Only the parse reports which rows it changed, via
    /// [`MinimapContent::note_syntax_rows`]. A change here therefore carries no
    /// row information and puts the sweep back to covering every built line.
    pub syntax_other: u64,
}

/// The built rows a pending recolor sweep has to re-summarize.
///
/// Accumulated between sweeps, so a burst of parses landing while one sweep is
/// still queued is covered by the union of what each changed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SweepRows {
    /// A change arrived with no row information, so every built row is suspect.
    All,
    /// Only these rows can have changed. Empty means none did.
    Rows(Range<u32>),
}

impl SweepRows {
    const NONE: SweepRows = SweepRows::Rows(0..0);

    /// Widen to also cover `rows`, or everything when either side is [`Self::All`].
    fn union(&mut self, rows: Option<Range<u32>>) {
        let (SweepRows::Rows(have), Some(add)) = (&*self, rows) else {
            *self = SweepRows::All;
            return;
        };
        if add.is_empty() {
            return;
        }
        *self = if have.is_empty() {
            SweepRows::Rows(add)
        } else {
            SweepRows::Rows(have.start.min(add.start)..have.end.max(add.end))
        };
    }
}

/// A buffer's diff and diagnostic marks, as the minimap needs them.
///
/// Resolving one row's mark is a seek into whatever the marks are stored in, so
/// a pass that asked every built row would seek once per line of the file. The
/// second method exists so such a pass can instead ask, in bulk, which rows are
/// even capable of carrying a mark.
///
/// Any `Fn(u32) -> Option<u8>` is an edge source. It answers that edge over the
/// plain syntax classes, and the bulk question with the conservative every-row
/// answer.
pub trait EdgeSource {
    /// The mark of `row`, [`RowMark::NONE`] when it carries none.
    fn mark_of(&self, row: u32) -> RowMark;

    /// The rows of `rows` that can carry a mark, in any order and without
    /// having to be deduplicated.
    ///
    /// A superset is correct and only costs time, so an implementation is free
    /// to answer from whatever ranges it has to hand rather than resolving each
    /// row. Omitting a row that does carry a mark is not: callers take the
    /// answer as settling that every other row is unmarked.
    fn marked_rows(&self, rows: Range<u32>) -> Vec<u32> {
        rows.collect()
    }
}

impl<F: Fn(u32) -> Option<u8>> EdgeSource for F {
    fn mark_of(&self, row: u32) -> RowMark {
        RowMark {
            edge: self(row),
            class_base: 0,
        }
    }
}

/// The edges of a row range, asked for once.
///
/// For a caller that both summarizes a range and records its marks, which
/// otherwise asks the source for each row twice.
struct ResolvedEdges {
    /// The marked rows and their marks, sorted by row.
    marks: Vec<(u32, RowMark)>,
}

impl ResolvedEdges {
    fn over(source: &impl EdgeSource, rows: Range<u32>) -> Self {
        // `marked_rows` answers a superset in any order, so the candidates are
        // narrowed and ordered here before each is asked for its class.
        let mut candidates = source.marked_rows(rows.clone());
        candidates.retain(|row| rows.contains(row));
        candidates.sort_unstable();
        candidates.dedup();

        Self {
            marks: candidates
                .into_iter()
                .map(|row| (row, source.mark_of(row)))
                .filter(|&(_, mark)| mark != RowMark::NONE)
                .collect(),
        }
    }
}

impl EdgeSource for ResolvedEdges {
    fn mark_of(&self, row: u32) -> RowMark {
        let index = self.marks.partition_point(|&(at, _)| at < row);
        self.marks
            .get(index)
            .filter(|&&(at, _)| at == row)
            .map_or(RowMark::NONE, |&(_, mark)| mark)
    }

    fn marked_rows(&self, rows: Range<u32>) -> Vec<u32> {
        let from = self.marks.partition_point(|&(row, _)| row < rows.start);
        let to = self.marks.partition_point(|&(row, _)| row < rows.end);
        self.marks[from..to].iter().map(|&(row, _)| row).collect()
    }
}

/// The run summaries of one buffer, plus the incremental-sync bookkeeping.
///
/// Mirrors the terminal's content store: one entry per line, spliced as the
/// buffer changes. Retains the rope it last synced against so an edit's old byte
/// range resolves to old rows, and a build cursor so a fresh file fills over
/// several frames.
pub struct MinimapContent {
    content_id: u32,
    /// Time one [`Self::sync`] spends on chunked work before it leaves the rest
    /// for the next one. [`Self::new`] takes [`SYNC_BUDGET`].
    budget: Duration,
    /// The executor whose clock measures [`Self::budget`], so a test scheduler
    /// decides when the budget ends a sync.
    executor: Executor,
    lines: Vec<LineRuns>,
    /// The built rows carrying a mark and the mark each carries, ascending by
    /// row. A row absent from it is unmarked.
    ///
    /// Sparse rather than one entry per built line so it doubles as the index of
    /// what is marked. A diff or diagnostic change re-summarizes the rows whose
    /// mark moved, and finding those means visiting what is marked now together
    /// with what [`EdgeSource::marked_rows`] says could become marked, neither
    /// of which is a walk of the file.
    edges: Vec<(u32, RowMark)>,
    synced_version: u64,
    synced_rope: Rope,
    built_upto: u32,
    disabled: bool,
    /// Combined diff and diagnostic version last synced. A change re-checks the
    /// built lines' edge marks without a buffer edit having occurred.
    synced_decoration_version: u64,
    /// Syntax-coloring version (toggle plus parse) last synced. A change
    /// re-summarizes the built lines' content without a buffer edit.
    synced_syntax_version: u64,
    /// Non-parse syntax version last synced, the [`SyncVersions::syntax_other`]
    /// half of [`Self::synced_syntax_version`]. A change to it has no row
    /// information behind it, so it widens the next sweep to every built row.
    synced_syntax_other: u64,
    /// The next built row a recolor sweep will re-summarize, or `None` when idle.
    /// A syntax version change starts the sweep at
    /// [`Self::pending_syntax_rows`]' start, and it advances in [`SYNC_SLICE`]
    /// steps until it reaches [`Self::resync_end`]. It rests there while
    /// [`Self::resync_extra`] still holds rows.
    resync_upto: Option<u32>,
    /// The row an active sweep's cursor stops at, or `None` to run to
    /// [`Self::built_upto`]. Always clamped to `built_upto` in use.
    resync_end: Option<u32>,
    /// The rows a parse reported while a sweep ran that lie outside the
    /// cursor's remaining window, swept once the cursor is done.
    ///
    /// The rows behind the cursor carry the old target's colors. Those differ
    /// from the new target's only on the rows the new parse reports. The rows
    /// ahead of the cursor read the new target when it gets there. So a parse
    /// that lands mid-sweep adds only its rows here, and a sweep still finishes
    /// while parses keep landing. `Some` only while [`Self::resync_upto`] is.
    resync_extra: Option<Range<u32>>,
    /// The syntax version an active sweep is bringing the strip to, so a fresh
    /// bump to a different version retargets the sweep in flight.
    resync_target: u64,
    /// Rows reported changed since a syntax bump last took them. The next bump
    /// hands them to the sweep it starts, or to the sweep in flight.
    pending_syntax_rows: SweepRows,
    /// The rows a deferred edit left holding stand-in summaries, which the
    /// same [`Self::sync`] hands to the recolor sweep.
    edited_rows: Option<Range<u32>>,
    /// The next built row the edge sweep will re-check, or `None` when idle.
    ///
    /// Separate cursor from [`Self::resync_upto`] because the two sweeps answer
    /// to different versions and a bump of one says nothing about the other. It
    /// advances in [`SYNC_SLICE`] steps, like the recolor sweep, so a branch
    /// switch on a large file does not re-mark every row in one frame.
    edge_resync_upto: Option<u32>,
    /// The decoration version an active edge sweep is bringing the strip to.
    ///
    /// A bump to a different version restarts the sweep at the top rather than
    /// widening it, since the rows already swept carry the old version's marks.
    edge_resync_target: u64,
    queued: Vec<Splice>,
}

impl MinimapContent {
    pub fn new(content_id: u32, executor: Executor) -> MinimapContent {
        MinimapContent::with_budget(content_id, executor, SYNC_BUDGET)
    }

    /// A content that spends `budget` per [`Self::sync`] in place of
    /// [`SYNC_BUDGET`], so a caller pins how far one call reaches.
    fn with_budget(content_id: u32, executor: Executor, budget: Duration) -> MinimapContent {
        MinimapContent {
            content_id,
            budget,
            executor,
            lines: Vec::new(),
            edges: Vec::new(),
            synced_version: 0,
            synced_rope: Rope::new(),
            built_upto: 0,
            disabled: false,
            synced_decoration_version: 0,
            synced_syntax_version: 0,
            synced_syntax_other: 0,
            resync_upto: None,
            resync_end: None,
            resync_extra: None,
            resync_target: 0,
            pending_syntax_rows: SweepRows::All,
            edited_rows: None,
            edge_resync_upto: None,
            edge_resync_target: 0,
            queued: Vec::new(),
        }
    }

    /// Record the buffer rows a completed parse changed the tokens of, or
    /// `None` when that is unknown and every built row must be re-summarized.
    ///
    /// Called once per parse install, before the sync that acts on it. Reports
    /// accumulate, so several parses landing between syncs are covered by the
    /// union of their rows rather than by whichever arrived last.
    pub fn note_syntax_rows(&mut self, rows: Option<Range<u32>>) {
        self.pending_syntax_rows.union(rows);
    }

    pub fn content_id(&self) -> u32 {
        self.content_id
    }

    /// The buffer version this content last synced against, the argument the
    /// caller passes to `edits_since` to collect the changes for the next sync.
    pub fn synced_version(&self) -> u64 {
        self.synced_version
    }

    /// Whether chunked work is still outstanding, from the initial build or
    /// either sweep.
    ///
    /// The caller ticks [`Self::sync`] on idle frames while this holds, so
    /// every cursor runs to completion instead of stalling until the next user
    /// event. A sweep needs this as much as the build does. It spends one
    /// budget per sync on a file that is already fully built, so without it
    /// every row past that budget's reach keeps stale colors or stale marks
    /// until some unrelated event happens to tick another sync.
    pub fn build_pending(&self) -> bool {
        !self.disabled
            && (self.built_upto < line_count(&self.synced_rope)
                || self.resync_upto.is_some()
                || self.edge_resync_upto.is_some())
    }

    /// Drain the pending splices for the emission layer.
    pub fn take_queued(&mut self) -> Vec<Splice> {
        std::mem::take(&mut self.queued)
    }

    /// Bring the summaries up to date with `new_rope` at `version`.
    ///
    /// `edits` is the buffer's `edits_since(self.synced_version())`. `tokens_for`
    /// resolves the syntax tokens of a row range, queried once per range each
    /// branch touches so a small edit never resolves the whole buffer, and
    /// `marks` a row's diff/diagnostic mark. [`SyncVersions::decoration`]
    /// changing re-checks the built lines' edge marks and [`SyncVersions::syntax`]
    /// changing re-summarizes their content, each without a buffer edit.
    ///
    /// Edits within the already-built prefix queue splices. A patch of more
    /// than [`EDIT_SUMMARIZE_MAX`] edits splices rows that stand in for the
    /// edited lines, and the recolor sweep re-summarizes them over the
    /// following calls. The unbuilt tail fills as far as [`Self::budget`]
    /// reaches per call. A buffer over [`MAX_LINES`] disables and queues
    /// nothing.
    pub fn sync(
        &mut self,
        new_rope: &Rope,
        version: u64,
        edits: &Patch<usize>,
        versions: SyncVersions,
        tokens_for: impl Fn(Range<u32>) -> HashMap<u32, Vec<LineToken>>,
        marks: impl EdgeSource,
    ) {
        if self.disabled {
            return;
        }

        let started = self.executor.now();
        let total = line_count(new_rope);
        if total as usize > MAX_LINES {
            self.disabled = true;
            self.lines.clear();
            self.edges.clear();
            self.queued.clear();
            self.built_upto = 0;
            self.synced_version = version;
            self.synced_decoration_version = versions.decoration;
            self.synced_syntax_version = versions.syntax;
            self.synced_rope = new_rope.clone();
            return;
        }

        if version != self.synced_version {
            if edits.edits().len() > EDIT_SUMMARIZE_MAX {
                self.splice_edits(edits, new_rope, &marks);
            } else {
                for edit in edits.edits() {
                    self.apply_edit(edit, new_rope, &tokens_for, &marks);
                }
            }
            self.synced_version = version;
            self.synced_rope = new_rope.clone();
        }

        // Every stage below reads the clock after its slice rather than before
        // it, so a sync that arrives with the budget already spent still
        // advances its cursor by one slice.
        while self.built_upto < total {
            let end = (self.built_upto + SYNC_SLICE).min(total);
            let tokens = tokens_for(self.built_upto..end);
            // Resolved once through the bulk path, since the summary below and
            // the record beside it both want every row's edge.
            let edges = ResolvedEdges::over(&marks, self.built_upto..end);
            let lines = summarize_rows(new_rope, self.built_upto..end, &tokens, &edges);
            self.queue_splice(Splice {
                start: self.built_upto,
                removed: 0,
                lines: lines.clone(),
            });
            self.edges.extend(edges.marks);
            self.lines.extend(lines);
            self.built_upto = end;

            if self.executor.now().duration_since(started) >= self.budget {
                break;
            }
        }

        // A change with no row information behind it can have restained any
        // line, so it widens whatever the parses have reported to everything.
        if versions.syntax_other != self.synced_syntax_other {
            self.pending_syntax_rows = SweepRows::All;
            self.synced_syntax_other = versions.syntax_other;
        }

        // A recolor re-summarizes the built lines' content, swept under the
        // budget so a large file never recolors in one frame. A syntax
        // version change starts the sweep over the rows reported changed since
        // the last one finished.
        if versions.syntax != self.synced_syntax_version
            && (self.resync_upto.is_none() || self.resync_target != versions.syntax)
        {
            let rows = std::mem::replace(&mut self.pending_syntax_rows, SweepRows::NONE);
            self.resync_target = versions.syntax;
            match (self.resync_upto, rows) {
                // A change with no rows behind it leaves every line suspect, so
                // the sweep starts over against the new target.
                (_, SweepRows::All) => {
                    self.resync_upto = Some(0);
                    self.resync_end = None;
                    self.resync_extra = None;
                },
                // A parse that lands mid-sweep keeps the cursor. The rows ahead
                // of it read the new target when it gets there. The rows behind
                // it differ from the new target only where the parse reports.
                // So only the reported rows outside the cursor's remaining
                // window go back on the sweep.
                (Some(upto), SweepRows::Rows(rows)) => {
                    let end = self.resync_end.unwrap_or(u32::MAX);
                    for part in [
                        rows.start..rows.end.min(upto),
                        rows.start.max(end)..rows.end,
                    ] {
                        if part.is_empty() {
                            continue;
                        }
                        self.resync_extra = Some(match self.resync_extra.take() {
                            Some(have) => have.start.min(part.start)..have.end.max(part.end),
                            None => part,
                        });
                    }
                },
                // Nothing changed, so the strip is already at the new version.
                (None, SweepRows::Rows(rows)) if rows.is_empty() => {
                    self.synced_syntax_version = versions.syntax;
                },
                (None, SweepRows::Rows(rows)) => {
                    self.resync_upto = Some(rows.start);
                    self.resync_end = Some(rows.end);
                },
            }
        }

        // Rows a deferred edit left on stand-in summaries join the recolor
        // sweep. A sweep of their own aims at the version the strip already
        // shows, so finishing it moves no version.
        if let Some(rows) = self.edited_rows.take() {
            let rows = rows.start.min(self.built_upto)..rows.end.min(self.built_upto);
            if !rows.is_empty() {
                match self.resync_upto {
                    None => {
                        self.resync_upto = Some(rows.start);
                        self.resync_end = Some(rows.end);
                        self.resync_target = self.synced_syntax_version;
                    },
                    Some(upto) => {
                        self.resync_upto = Some(upto.min(rows.start));
                        self.resync_end = self.resync_end.map(|end| end.max(rows.end));
                    },
                }
            }
        }
        if let Some(from) = self.resync_upto {
            let end = self
                .resync_end
                .unwrap_or(self.built_upto)
                .min(self.built_upto);
            let mut from = from.min(end);

            // The loop sweeps the cursor's window first and the rows held for
            // the end after it. Both go one slice at a time under one budget.
            loop {
                let slice = if from < end {
                    let to = from.saturating_add(SYNC_SLICE).min(end);
                    let slice = from..to;
                    from = to;
                    slice
                } else if let Some(slice) = self.next_extra_slice() {
                    slice
                } else {
                    break;
                };
                self.resync_slice(new_rope, slice, &tokens_for);

                if self.executor.now().duration_since(started) >= self.budget {
                    break;
                }
            }

            if from >= end && self.resync_extra.is_none() {
                self.resync_upto = None;
                self.synced_syntax_version = self.resync_target;
            } else {
                self.resync_upto = Some(from);
            }
        }

        // A diff or diagnostic change re-checks every built line's edge mark,
        // independent of the content sweep. The sweep above paints the marks as
        // of the last decoration sync, so a row whose mark this bump moves is
        // re-spliced here even when the sweep just touched it.
        //
        // Sliced like the recolor sweep, since a branch switch can move a mark
        // on every row of the file at once.
        if versions.decoration != self.synced_decoration_version
            && (self.edge_resync_upto.is_none() || self.edge_resync_target != versions.decoration)
        {
            self.edge_resync_target = versions.decoration;
            self.edge_resync_upto = Some(0);
        }
        if let Some(from) = self.edge_resync_upto {
            let end = self.built_upto;
            let mut from = from.min(end);

            loop {
                let to = from.saturating_add(SYNC_SLICE).min(end);
                self.resync_edges(new_rope, &tokens_for, &marks, from..to);
                from = to;

                if from >= end || self.executor.now().duration_since(started) >= self.budget {
                    break;
                }
            }

            if from >= end {
                self.edge_resync_upto = None;
                self.synced_decoration_version = self.edge_resync_target;
            } else {
                self.edge_resync_upto = Some(from);
            }
        }
    }

    /// The next slice of [`Self::resync_extra`] within the built rows, taken off
    /// its front, or `None` once none of it is left.
    fn next_extra_slice(&mut self) -> Option<Range<u32>> {
        let extra = self.resync_extra.take()?;
        let end = extra.end.min(self.built_upto);
        if extra.start >= end {
            return None;
        }

        let to = extra.start.saturating_add(SYNC_SLICE).min(end);
        if to < end {
            self.resync_extra = Some(to..end);
        }
        Some(extra.start..to)
    }

    /// Re-summarize the built lines in `range` and queue a one-line splice where
    /// a line's full summary changed, for a recolor (highlight toggle or a
    /// completed parse) that leaves the buffer text untouched.
    ///
    /// A recolor can touch any line, so the caller sweeps the whole built range
    /// one [`SYNC_SLICE`] at a time, over as many syncs as the budget takes.
    ///
    /// Each row keeps the mark it already carries, read from the stored index
    /// rather than resolved again. A recolor cannot move a mark, and asking the
    /// source would cost a seek per line of the file for an answer it already
    /// holds. A decoration bump landing in the same sync makes those marks one
    /// version stale, which [`Self::resync_edges`] corrects immediately after.
    fn resync_slice(
        &mut self,
        new_rope: &Rope,
        range: Range<u32>,
        tokens_for: &impl Fn(Range<u32>) -> HashMap<u32, Vec<LineToken>>,
    ) {
        let tokens = tokens_for(range.clone());
        let mut text = String::new();
        let mut walk = new_rope.line_walk(range.clone());
        for row in range {
            text.clear();
            walk.next_into(&mut text);
            let summary = summarize_line(&text, row_tokens(&tokens, row), self.mark_at(row));
            if self.lines[row as usize] == summary {
                continue;
            }
            self.replace_line(row, summary);
        }
    }

    /// Re-check every built line's edge mark, re-summarizing and queueing a
    /// one-line splice only where the mark changed.
    ///
    /// The syntax content is unaffected by a diff or diagnostic change, so only
    /// the changed lines pay for a re-summarize.
    ///
    /// Which rows changed is settled first, from `marks` alone, so `tokens_for`
    /// is called only over the runs that actually need re-summarizing. This runs
    /// on every decoration bump, and a diff recompute or diagnostic batch
    /// usually moves no mark at all, so resolving tokens for the whole built
    /// range up front would be the dominant cost of doing nothing.
    ///
    /// A row that carries a mark now can have changed, and so can one
    /// [`EdgeSource::marked_rows`] says could come to carry one. Every other row
    /// was unmarked and stays unmarked, so the pass costs the marks rather than
    /// the file.
    fn resync_edges(
        &mut self,
        new_rope: &Rope,
        tokens_for: &impl Fn(Range<u32>) -> HashMap<u32, Vec<LineToken>>,
        marks: &impl EdgeSource,
        window: Range<u32>,
    ) {
        let suspect = {
            let mut rows = marks.marked_rows(window.clone());
            // Sliced by partition point rather than filtered, so a window costs
            // its own marks rather than every mark in the file.
            let from = self.edges.partition_point(|&(row, _)| row < window.start);
            let to = self.edges.partition_point(|&(row, _)| row < window.end);
            rows.extend(self.edges[from..to].iter().map(|&(row, _)| row));
            rows.retain(|row| window.contains(row));
            rows.sort_unstable();
            rows.dedup();
            rows
        };

        let changed: Vec<(u32, RowMark)> = suspect
            .into_iter()
            .filter_map(|row| {
                let mark = marks.mark_of(row);
                (self.mark_at(row) != mark).then_some((row, mark))
            })
            .collect();
        if changed.is_empty() {
            return;
        }

        self.apply_edge_changes(window, &changed);

        let mut text = String::new();
        for run in contiguous_runs(&changed) {
            let rows = changed[run].iter();
            let span = rows.clone().next().expect("a run is non-empty").0
                ..rows.clone().next_back().expect("a run is non-empty").0 + 1;
            let tokens = tokens_for(span.clone());

            // A run's rows are consecutive, so one walk over its span advances
            // in step with them.
            let mut walk = new_rope.line_walk(span);
            for &(row, mark) in rows {
                text.clear();
                walk.next_into(&mut text);
                let summary = summarize_line(&text, row_tokens(&tokens, row), mark);
                self.replace_line(row, summary);
            }
        }
    }

    /// Apply one edit that falls inside the built prefix, re-summarizing the new
    /// rows and shifting the build and recolor-sweep cursors by the line delta.
    ///
    /// An edit starting past the build cursor is left for the chunked build to
    /// summarize fresh when it reaches those rows.
    ///
    /// An edit that starts inside the prefix but reaches past the cursor keeps
    /// every row it inserted. It summarizes all of them and the cursor advances
    /// by the same line delta, so the rows the store holds and the rows the
    /// cursor claims stay equal, and the build picks up at the row after the
    /// edit's last.
    ///
    /// `removed` is clamped to the prefix for the same reason. The store cannot
    /// lose a row it never held, so the clamp is what keeps the count the splice
    /// reports equal to what the store gave up.
    fn apply_edit(
        &mut self,
        edit: &Edit<usize>,
        new_rope: &Rope,
        tokens_for: &impl Fn(Range<u32>) -> HashMap<u32, Vec<LineToken>>,
        marks: &impl EdgeSource,
    ) {
        // The earlier edits of this patch already spliced the rows under this
        // one, so the row it replaces has moved by their line delta. The new
        // rope's prefix differs from the old one's by exactly that much, which
        // makes its own start row the index the store wants.
        let new_start_row = new_rope.offset_to_point(edit.new.start).row;
        if new_start_row >= self.built_upto {
            return;
        }
        let new_end_row = new_rope.offset_to_point(edit.new.end).row;

        let old_start_row = self.synced_rope.offset_to_point(edit.old.start).row;
        let old_end_row = self.synced_rope.offset_to_point(edit.old.end).row;

        let tokens = tokens_for(new_start_row..new_end_row + 1);

        // A shift moves where the replaced rows sit, not how many there are.
        let replaced = old_end_row + 1 - old_start_row;
        let removed = (new_start_row + replaced).min(self.built_upto) - new_start_row;
        let inserted = summarize_rows(new_rope, new_start_row..new_end_row + 1, &tokens, marks);
        let inserted_marks: Vec<RowMark> = (new_start_row..new_end_row + 1)
            .map(|row| marks.mark_of(row))
            .collect();

        let start = new_start_row as usize;
        let end = (start + removed as usize).min(self.lines.len());
        self.lines.splice(start..end, inserted.iter().cloned());
        self.splice_edges(new_start_row..end as u32, &inserted_marks);

        let delta = inserted.len() as i64 - removed as i64;
        self.built_upto = (self.built_upto as i64 + delta).max(0) as u32;
        self.slide_sweep_bounds(new_start_row, replaced, delta);

        self.queue_splice(Splice {
            start: new_start_row,
            removed,
            lines: inserted,
        });
    }

    /// Slide the recolor sweep's bounds and the rows the parses reported past
    /// an edit that replaced `replaced` store rows from `new_start_row` and
    /// moved the rows after them by `delta`.
    fn slide_sweep_bounds(&mut self, new_start_row: u32, replaced: u32, delta: i64) {
        // Sweep bounds are absolute rows, so rows the edit slid underneath them
        // would never be swept and would keep their pre-recolor runs. Rows the
        // edit re-summarized already carry current tokens, so resuming at the
        // seam of a straddling edit re-sweeps them idempotently.
        //
        // `resync_end` of `None` runs to `built_upto`, which the splice
        // already moved, so only a bounded end needs sliding.
        //
        // The bounds are the rows the edit replaced as the store held them, so
        // they read from the new start rather than the old.
        let edit_last_row = new_start_row + replaced - 1;
        let slide = |row: u32| {
            if edit_last_row < row {
                (row as i64 + delta).max(new_start_row as i64) as u32
            } else if new_start_row < row {
                new_start_row
            } else {
                row
            }
        };
        if let Some(upto) = self.resync_upto {
            self.resync_upto = Some(slide(upto));
            self.resync_end = self.resync_end.map(slide);
            self.resync_extra = self
                .resync_extra
                .take()
                .map(|rows| slide(rows.start)..slide(rows.end));
        }
        if let SweepRows::Rows(rows) = &self.pending_syntax_rows
            && !rows.is_empty()
        {
            self.pending_syntax_rows = SweepRows::Rows(slide(rows.start)..slide(rows.end));
        }
    }

    /// Apply a patch of many edits in one pass over the store, giving each new
    /// row a stand-in summary and leaving the fresh ones to the recolor sweep.
    ///
    /// A new row stands in with the row it replaced, or with the last row its
    /// edit replaced when the edit inserted more rows than it removed. A
    /// stand-in is always a row the terminal holds, so an edit that keeps its
    /// row count changes nothing the terminal shows and queues no splice. The
    /// new rows land in [`Self::edited_rows`] for [`Self::sync`] to sweep.
    ///
    /// Rows, marks, splices, and cursors end where [`Self::apply_edit`] per
    /// edit leaves them, apart from the summaries the sweep has yet to reach.
    fn splice_edits(&mut self, edits: &Patch<usize>, new_rope: &Rope, marks: &impl EdgeSource) {
        let edits = edits.edits();
        let new_points = new_rope.offsets_to_points_batch(
            &edits
                .iter()
                .flat_map(|edit| [edit.new.start, edit.new.end])
                .collect::<Vec<_>>(),
        );
        let old_points = self.synced_rope.offsets_to_points_batch(
            &edits
                .iter()
                .flat_map(|edit| [edit.old.start, edit.old.end])
                .collect::<Vec<_>>(),
        );

        let built = self.lines.len();
        let mut lines: Vec<LineRuns> = Vec::with_capacity(built);
        let mut edges: Vec<(u32, RowMark)> = Vec::with_capacity(self.edges.len());
        // The first old row, and the first old mark, not yet copied or replaced.
        let mut old_row = 0;
        let mut old_edge = 0;
        let mut built_upto = self.built_upto;

        for (old, new) in old_points.chunks(2).zip(new_points.chunks(2)) {
            let (old_start_row, old_end_row) = (old[0].row, old[1].row);
            // The chunked build summarizes the rows past the cursor fresh.
            if old_start_row as usize >= built {
                break;
            }
            let (new_start_row, new_end_row) = (new[0].row, new[1].row);

            let replaced = old_end_row + 1 - old_start_row;
            let removed = (new_start_row + replaced).min(built_upto) - new_start_row;
            let inserted = new_end_row + 1 - new_start_row;

            let start = new_start_row as usize;
            if lines.len() < start {
                let count = start - lines.len();
                let shift = lines.len() as i64 - old_row as i64;
                lines.extend_from_slice(&self.lines[old_row..old_row + count]);
                old_row += count;
                while let Some(&(row, mark)) = self.edges.get(old_edge)
                    && (row as usize) < old_row
                {
                    edges.push(((row as i64 + shift) as u32, mark));
                    old_edge += 1;
                }
            }

            // An earlier edit ending on this edit's first row already emitted
            // that row, and this edit replaces it, as the same edit in
            // `apply_edit` splices it out again.
            let reclaimed = lines.split_off(start);
            while edges.last().is_some_and(|&(row, _)| row >= new_start_row) {
                edges.pop();
            }

            let stand_ins: Vec<LineRuns> = {
                let replaced_row = |at: usize| match reclaimed.get(at) {
                    Some(&summary) => summary,
                    None => self.lines[old_row + at - reclaimed.len()],
                };
                let last = removed as usize - 1;
                (0..inserted as usize)
                    .map(|i| replaced_row(i.min(last)))
                    .collect()
            };
            old_row += removed as usize - reclaimed.len();
            while self
                .edges
                .get(old_edge)
                .is_some_and(|&(row, _)| (row as usize) < old_row)
            {
                old_edge += 1;
            }

            lines.extend_from_slice(&stand_ins);
            edges.extend(
                (new_start_row..new_end_row + 1)
                    .map(|row| (row, marks.mark_of(row)))
                    .filter(|&(_, mark)| mark != RowMark::NONE),
            );

            let delta = inserted as i64 - removed as i64;
            built_upto = (built_upto as i64 + delta) as u32;
            self.slide_sweep_bounds(new_start_row, replaced, delta);
            self.edited_rows = Some(match self.edited_rows.take() {
                Some(rows) => rows.start.min(new_start_row)..rows.end.max(new_end_row + 1),
                None => new_start_row..new_end_row + 1,
            });

            if delta != 0 {
                self.queue_splice(Splice {
                    start: new_start_row,
                    removed,
                    lines: stand_ins,
                });
            }
        }

        let shift = lines.len() as i64 - old_row as i64;
        lines.extend_from_slice(&self.lines[old_row..]);
        edges.extend(
            self.edges[old_edge..]
                .iter()
                .map(|&(row, mark)| ((row as i64 + shift) as u32, mark)),
        );

        self.lines = lines;
        self.edges = edges;
        self.built_upto = built_upto;
    }

    /// Store `summary` as `row`'s and queue the one-line splice carrying it.
    ///
    /// The store and the payload are two copies of the runs. The wire replay
    /// consumes the payload, while the store stays to answer the next sync's
    /// comparison, and a [`LineRuns`] copies rather than allocating.
    fn replace_line(&mut self, row: u32, summary: LineRuns) {
        self.lines[row as usize] = summary;

        self.queue_splice(Splice {
            start: row,
            removed: 1,
            lines: vec![summary],
        });
    }

    /// The mark `row` carries, [`RowMark::NONE`] when it is unmarked.
    fn mark_at(&self, row: u32) -> RowMark {
        self.edges
            .binary_search_by_key(&row, |&(r, _)| r)
            .map_or(RowMark::NONE, |at| self.edges[at].1)
    }

    /// Record every change in `changed`, which must be ascending by row and
    /// confined to `window`.
    ///
    /// A handful go in one at a time. Past that the window's slice of the edge
    /// list is rebuilt in one pass and spliced back, because each
    /// [`Self::set_mark`] shifts every entry after the one it touches, and a
    /// branch switch changes marks by the thousand.
    fn apply_edge_changes(&mut self, window: Range<u32>, changed: &[(u32, RowMark)]) {
        if changed.len() < EDGE_BATCH_MIN {
            for &(row, mark) in changed {
                self.set_mark(row, mark);
            }
            return;
        }

        let from = self.edges.partition_point(|&(row, _)| row < window.start);
        let to = self.edges.partition_point(|&(row, _)| row < window.end);

        let mut merged: Vec<(u32, RowMark)> = Vec::with_capacity(to - from + changed.len());
        let mut old = self.edges[from..to].iter().peekable();
        for &(row, mark) in changed {
            while old.peek().is_some_and(|&&(r, _)| r < row) {
                merged.push(*old.next().expect("peeked"));
            }
            // The row's own entry, if it had one, is what `mark` replaces.
            if old.peek().is_some_and(|&&(r, _)| r == row) {
                old.next();
            }
            if mark != RowMark::NONE {
                merged.push((row, mark));
            }
        }
        merged.extend(old.copied());
        self.edges.splice(from..to, merged);
    }

    /// Record that `row` now carries `mark`, dropping it from the marked rows
    /// when that is [`RowMark::NONE`].
    fn set_mark(&mut self, row: u32, mark: RowMark) {
        let at = self.edges.binary_search_by_key(&row, |&(r, _)| r);
        match (at, mark == RowMark::NONE) {
            (Ok(at), false) => self.edges[at].1 = mark,
            (Ok(at), true) => {
                self.edges.remove(at);
            },
            (Err(at), false) => self.edges.insert(at, (row, mark)),
            (Err(_), true) => {},
        }
    }

    /// Replace the marks of `rows` with `inserted`'s, one entry per row from
    /// `rows.start`, sliding the rows after `rows` by the line delta.
    ///
    /// Mirrors the splice [`Self::lines`] takes for the same edit, so the two
    /// keep agreeing on which row is which.
    fn splice_edges(&mut self, rows: Range<u32>, inserted: &[RowMark]) {
        let delta = inserted.len() as i64 - (rows.end - rows.start) as i64;
        let from = self.edges.partition_point(|&(row, _)| row < rows.start);
        let to = self.edges.partition_point(|&(row, _)| row < rows.end);

        for (row, _) in &mut self.edges[to..] {
            *row = (*row as i64 + delta).max(0) as u32;
        }
        self.edges.splice(
            from..to,
            inserted
                .iter()
                .enumerate()
                .filter(|&(_, &mark)| mark != RowMark::NONE)
                .map(|(i, &mark)| (rows.start + i as u32, mark)),
        );
    }

    /// Queue `splice`, folding it into the pending tail when the two describe
    /// one contiguous run.
    ///
    /// The sweeps walk rows in order and queue a splice per changed row, so
    /// without this a full recolor ships thousands of one-line APC frames, each
    /// paying its own framing, a journal clone, and an O(store-tail) memmove on
    /// the terminal.
    ///
    /// Only 1:1 replacements merge. Two contiguous ones replace `start ..
    /// start + r1 + r2` between them, which is exactly what the merged splice
    /// does. A build splice inserts without removing, so folding a later splice
    /// into it would remove rows the pair never touched -- requiring 1:1 on the
    /// tail as well as the incoming splice is what rules that out.
    fn queue_splice(&mut self, splice: Splice) {
        let mergeable = self.queued.last().is_some_and(|tail| {
            tail.removed as usize == tail.lines.len()
                && splice.removed as usize == splice.lines.len()
                && splice.start == tail.start + tail.lines.len() as u32
        });

        match self.queued.last_mut() {
            Some(tail) if mergeable => {
                tail.removed += splice.removed;
                tail.lines.extend(splice.lines);
            },
            _ => self.queued.push(splice),
        }
    }
}

/// Index ranges of `changed` splitting it into runs of consecutive rows.
///
/// `changed` is ascending by row. Marks usually land in clusters, so grouping
/// them lets one token fetch cover a whole run instead of one fetch per row.
fn contiguous_runs(changed: &[(u32, RowMark)]) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start = 0;
    for i in 1..changed.len() {
        if changed[i].0 != changed[i - 1].0 + 1 {
            runs.push(start..i);
            start = i;
        }
    }
    if !changed.is_empty() {
        runs.push(start..changed.len());
    }
    runs
}

/// Summarize each row in `rows`, its tokens from `tokens` and its mark from
/// `marks`.
fn summarize_rows(
    rope: &Rope,
    rows: Range<u32>,
    tokens: &HashMap<u32, Vec<LineToken>>,
    marks: &impl EdgeSource,
) -> Vec<LineRuns> {
    let mut text = String::new();
    let mut walk = rope.line_walk(rows.clone());
    rows.map(|row| {
        // A row the walk has run out for summarizes as empty, which is what
        // reading it a row at a time produced. Skipping it instead would shift
        // every later summary onto the wrong row.
        text.clear();
        walk.next_into(&mut text);
        summarize_line(&text, row_tokens(tokens, row), marks.mark_of(row))
    })
    .collect()
}

/// The tokens covering `row`, empty for a row the resolver reported none for.
fn row_tokens(tokens: &HashMap<u32, Vec<LineToken>>, row: u32) -> &[LineToken] {
    tokens.get(&row).map_or(&[], Vec::as_slice)
}

/// Total line count of `rope`, counting a trailing empty line.
fn line_count(rope: &Rope) -> u32 {
    rope.max_point().row + 1
}

/// Compress `line` into colored run blocks, prefixed by an optional edge mark.
///
/// When `mark` carries an edge, a run fills the reserved [`LANE_WIDTH`]-column
/// lane at the left in that class, and the content summary starts after it so
/// the mark never overwrites the code silhouette.
///
/// Content walks the line by display column, a tab advancing to the next multiple
/// of [`TAB_WIDTH`] and other chars advancing one, capped at [`MAX_COLUMNS`]. A
/// non-whitespace char extends the current run when it is contiguous and shares
/// both the covering token's class and the glyph weight, otherwise opens a new
/// run. Whitespace ends the current run, so a gap breaks the blocks. A char
/// uncovered by any token is class 0. Once [`MAX_RUNS`] runs exist, the last run
/// swallows the rest of the line.
///
/// Every content class takes the mark's `class_base`. Under an offset, class 0 is
/// therefore the uncovered-text color of the row's population block.
pub fn summarize_line(line: &str, tokens: &[LineToken], mark: RowMark) -> LineRuns {
    let mut runs = LineRuns::default();
    if let Some(class) = mark.edge {
        runs.push(Run {
            start_col: 0,
            len: LANE_WIDTH as u8,
            class,
            // A mark rather than a glyph, so it carries no glyph's weight and
            // draws at full strength.
            weight: 255,
        });
    }
    let mut col: u32 = LANE_WIDTH;
    let mut byte: usize = 0;
    let mut token_idx = 0;
    let mut overflowed = false;

    for ch in line.chars() {
        if col >= MAX_COLUMNS {
            break;
        }
        let raw_width = if ch == '\t' {
            TAB_WIDTH - (col % TAB_WIDTH)
        } else {
            1
        };
        let width = raw_width.min(MAX_COLUMNS - col);

        if !ch.is_whitespace() {
            while token_idx < tokens.len() && tokens[token_idx].range.end <= byte {
                token_idx += 1;
            }
            let class = mark.class_base
                + match tokens.get(token_idx) {
                    Some(token) if token.range.start <= byte => token.class,
                    _ => 0,
                };

            let weight = glyph_weight(ch);
            let contiguous = runs.last().is_some_and(|last| {
                last.class == class
                    && last.weight == weight
                    && last.start_col as u32 + last.len as u32 == col
            });
            if overflowed || contiguous {
                let last = runs.last_mut().expect("a run to extend");
                last.len = (col + width - last.start_col as u32) as u8;
            } else if runs.len() < MAX_RUNS {
                runs.push(Run {
                    start_col: col as u8,
                    len: width as u8,
                    class,
                    weight,
                });
            } else {
                overflowed = true;
                let last = runs.last_mut().expect("a run to extend");
                last.len = (col + width - last.start_col as u32) as u8;
            }
        }

        col += width;
        byte += ch.len_utf8();
    }

    runs
}

/// How much ink `ch` puts on the page, as a run's weight.
///
/// A strip draws a line a couple of pixels tall, far under what a letter needs,
/// so a run carries the weight of its glyphs rather than their shapes. Capitals
/// and digits fill the line's band, other letters sit within the x height, and
/// punctuation is a mark in the middle of one. Without the split a line of code
/// and a line of underscores cover the same pixels at the same strength.
fn glyph_weight(ch: char) -> u8 {
    if ch.is_uppercase() || ch.is_numeric() {
        255
    } else if ch.is_alphabetic() {
        190
    } else {
        110
    }
}

/// Maps a buffer's syntax highlight styles to compact minimap classes and the
/// palette they draw in.
///
/// Class 0 is the theme's default foreground for uncovered text. Class `i + 1` is
/// the resolved foreground of the `i`-th syntax scope. The emission layer declares
/// [`Self::palette`] on the strip, and a token's [`HighlightStyleId`] resolves to
/// its class via [`Self::class_of`]. A span whose style was resolved to a color
/// resolves through [`Self::class_of_fg`].
///
/// A class is therefore a theme-key index, which is the same index under every
/// theme: the key set is static, and
/// [`HighlightStyleInterner::push`](crate::display_map::highlights::HighlightStyleInterner::push)
/// never dedups, so an id recorded under one theme names the same key under the
/// next. A minimap store built under one theme is valid under every other, and a
/// switch re-declares the palette rather than rebuilding what the strip holds.
///
/// The palette lays out the syntax block of [`Self::stride`] classes, then the
/// seven edge classes, then four copies of the syntax block for added,
/// modified, moved, and deleted rows. A row's [`RowMark::class_base`] picks its
/// block. The first `stride + 6` entries are exactly the palette a terminal
/// before protocol 13 read, and the header class follows them inside that
/// terminal's 64-entry cap. So [`Self::legacy_palette`] is a prefix, and the
/// edge classes keep their numbers.
pub struct ClassTable {
    palette: Vec<[u8; 3]>,
    by_style: HashMap<HighlightStyleId, u8>,
    by_fg: HashMap<[u8; 3], u8>,
    edge_base: u8,
    population_base: u8,
}

impl ClassTable {
    pub fn from_theme(theme: &Theme) -> ClassTable {
        let styles = SyntaxStyles::from_theme(theme);
        let default_fg = color_to_rgb(theme.get("ui.text").fg.unwrap_or(Color::White));

        let mut palette = vec![default_fg];
        let mut by_style = HashMap::new();
        let mut by_fg = HashMap::new();
        for index in 0..styles.theme_keys().len() {
            let Some(style_id) = styles.id_for_highlight(HighlightId(index as u32)) else {
                palette.push(default_fg);
                continue;
            };
            let class = (index + 1) as u8;
            by_style.insert(style_id, class);
            let fg = styles.interner[style_id].foreground;
            if let Some(rgb) = fg.map(color_to_rgb).filter(|&rgb| rgb != default_fg) {
                by_fg.entry(rgb).or_insert(class);
            }
            palette.push(color_to_rgb(fg.unwrap_or(Color::White)));
        }

        // The edge classes follow the syntax scopes, in EdgeClass order, so a
        // run's class indexes its mark color directly.
        let edge_base = palette.len() as u8;
        for scope in [
            scope::DIFF_ADDED,
            scope::DIFF_DELETED,
            scope::DIFF_MODIFIED,
            scope::UI_DIAGNOSTIC_ERROR,
            scope::UI_DIAGNOSTIC_WARNING,
            scope::UI_DIAGNOSTIC_INFO,
            scope::VCS_COMMIT_SHA,
        ] {
            palette.push(color_to_rgb(theme.get(scope).fg.unwrap_or(Color::White)));
        }

        // The population blocks repeat the syntax block, in the order
        // `population_base` and `deleted_base` answer them.
        let population_base = palette.len() as u8;
        for _ in 0..4 {
            palette.extend_from_within(..edge_base as usize);
        }

        ClassTable {
            palette,
            by_style,
            by_fg,
            edge_base,
            population_base,
        }
    }

    /// The palette class the edge mark `kind` draws in.
    ///
    /// The seven edge classes occupy the palette right after the syntax scopes,
    /// in [`EdgeClass`] order, so the returned class indexes the mark's declared
    /// color on the strip.
    pub fn edge_class(&self, kind: EdgeClass) -> u8 {
        self.edge_base + kind as u8
    }

    /// The number of classes in one syntax block, the default foreground and
    /// one per syntax scope. Each population block repeats that many.
    pub fn stride(&self) -> u8 {
        self.edge_base
    }

    /// The offset a row in `status` gives its content classes.
    ///
    /// An unchanged row takes the plain syntax block at 0. An added, modified,
    /// or moved row takes its own copy of that block after the edge classes.
    pub fn population_base(&self, status: DiffStatus) -> u8 {
        let stride = self.stride();
        match status {
            DiffStatus::Unchanged => 0,
            DiffStatus::Added => self.population_base,
            DiffStatus::Modified => self.population_base + stride,
            DiffStatus::Moved => self.population_base + 2 * stride,
        }
    }

    /// The offset a base-only row gives its content classes.
    ///
    /// A deleted line shown from the base has no buffer row, so [`DiffStatus`]
    /// has no variant for it.
    pub fn deleted_base(&self) -> u8 {
        self.population_base + 3 * self.stride()
    }

    /// The palette a terminal before protocol 13 reads, the syntax block and the
    /// edge classes with no population block.
    pub fn legacy_palette(&self) -> &[[u8; 3]] {
        &self.palette[..self.population_base as usize]
    }

    /// The class a token drawn in `style` maps to, or 0 when the style is not a
    /// recognized syntax scope.
    pub fn class_of(&self, style: HighlightStyleId) -> u8 {
        self.by_style.get(&style).copied().unwrap_or(0)
    }

    /// The class a span drawn in the resolved foreground `fg` maps to, or 0 when
    /// no theme key paints in that color.
    ///
    /// This is the lookup for a span whose style was resolved before it reached
    /// the strip, as [`BaseHighlights`](crate::diff_map::BaseHighlights) spans
    /// are. The class draws in the span's own color, because a class's palette
    /// entry is that color. Keys that share a color share the first key's class,
    /// and the default foreground maps to class 0, as uncovered text does.
    pub fn class_of_fg(&self, fg: Option<Color>) -> u8 {
        fg.and_then(|fg| self.by_fg.get(&color_to_rgb(fg)).copied())
            .unwrap_or(0)
    }

    /// The rgb color of each class, indexed by class.
    pub fn palette(&self) -> &[[u8; 3]] {
        &self.palette
    }
}

/// Resolve a terminal color to rgb, falling back to a mid gray for indexed or
/// reset colors a minimap has no palette for.
pub(crate) fn color_to_rgb(color: Color) -> [u8; 3] {
    match color {
        Color::Rgb(r, g, b) => [r, g, b],
        Color::Black => [0, 0, 0],
        Color::Red => [205, 0, 0],
        Color::Green => [0, 205, 0],
        Color::Yellow => [205, 205, 0],
        Color::Blue => [0, 0, 238],
        Color::Magenta => [205, 0, 205],
        Color::Cyan => [0, 205, 205],
        Color::Gray => [229, 229, 229],
        Color::DarkGray => [127, 127, 127],
        Color::LightRed => [255, 0, 0],
        Color::LightGreen => [0, 255, 0],
        Color::LightYellow => [255, 255, 0],
        Color::LightBlue => [92, 92, 255],
        Color::LightMagenta => [255, 0, 255],
        Color::LightCyan => [0, 255, 255],
        Color::White => [255, 255, 255],
        _ => [200, 200, 200],
    }
}

#[cfg(test)]
mod tests {
    use super::{
        summarize_line, EdgeSource, LineRuns, LineToken, MinimapContent, RowMark, Run, Splice,
        SyncVersions, MAX_LINES, MAX_RUNS, SYNC_BUDGET, SYNC_SLICE,
    };
    use std::{cell::RefCell, collections::HashMap, ops::Range, sync::Arc, time::Duration};
    use stoat_scheduler::{Executor, TestScheduler};
    use stoat_text::{
        patch::{Edit, Patch},
        Rope,
    };

    fn rope(text: &str) -> Rope {
        Rope::from(text)
    }

    /// An executor on a test clock, which moves only when a test moves it.
    fn test_executor() -> Executor {
        Executor::new(Arc::new(TestScheduler::new()))
    }

    fn no_tokens(_: Range<u32>) -> HashMap<u32, Vec<LineToken>> {
        HashMap::new()
    }

    fn no_edges(_: u32) -> Option<u8> {
        None
    }

    /// An edge source that marks a fixed set of rows and records every row it
    /// is asked about, so how far a pass reaches can be asserted directly.
    #[derive(Clone, Copy)]
    struct RecordingEdges<'a> {
        marked: &'a [u32],
        asked: &'a RefCell<Vec<u32>>,
    }

    impl EdgeSource for RecordingEdges<'_> {
        fn mark_of(&self, row: u32) -> RowMark {
            self.asked.borrow_mut().push(row);
            RowMark {
                edge: self.marked.contains(&row).then_some(40),
                class_base: 0,
            }
        }

        fn marked_rows(&self, rows: Range<u32>) -> Vec<u32> {
            self.marked
                .iter()
                .copied()
                .filter(|row| rows.contains(row))
                .collect()
        }
    }

    /// An edge source that fails on any use, so a pass that must consult none
    /// is pinned by whether it runs at all rather than by a count.
    struct UnusedEdges;

    impl EdgeSource for UnusedEdges {
        fn mark_of(&self, row: u32) -> RowMark {
            panic!("row {row} was asked about");
        }

        fn marked_rows(&self, rows: Range<u32>) -> Vec<u32> {
            panic!("rows {rows:?} were asked about");
        }
    }

    /// A bump carrying no row information, so the sweep covers every built row.
    /// Moving `syntax_other` in step with `syntax` is what makes it one.
    fn versions(decoration: u64, syntax: u64) -> SyncVersions {
        SyncVersions {
            decoration,
            syntax,
            syntax_other: syntax,
        }
    }

    /// A bump from a parse alone, so the sweep uses whatever rows
    /// [`MinimapContent::note_syntax_rows`] reported. Holding `syntax_other`
    /// still across calls is what makes it a parse-only bump.
    fn parse_versions(decoration: u64, syntax: u64) -> SyncVersions {
        SyncVersions {
            decoration,
            syntax,
            syntax_other: 7,
        }
    }

    fn run(start_col: u8, len: u8, class: u8) -> Run {
        weighted(start_col, len, class, 190)
    }

    fn weighted(start_col: u8, len: u8, class: u8, weight: u8) -> Run {
        Run {
            start_col,
            len,
            class,
            weight,
        }
    }

    /// A line holds `MAX_RUNS` runs and drops what comes after.
    ///
    /// The store is a fixed array, so a push past the end has nowhere to go.
    /// Dropping it is what the summary already does. `summarize_line` widens
    /// its last run to swallow the rest of the line rather than pushing again,
    /// and a push that reached here has passed that decision.
    #[test]
    fn a_line_holds_max_runs_and_drops_the_rest() {
        let mut runs = LineRuns::default();
        for i in 0..MAX_RUNS {
            runs.push(run(i as u8, 1, i as u8));
        }
        assert_eq!(runs.len(), MAX_RUNS, "the line fills to the cap");

        runs.push(run(99, 1, 99));
        assert_eq!(runs.len(), MAX_RUNS, "and the next push adds nothing");
        assert_eq!(
            runs.as_slice().last(),
            Some(&run(MAX_RUNS as u8 - 1, 1, MAX_RUNS as u8 - 1)),
            "the run it dropped never displaced one that fit",
        );
    }

    fn tok(range: Range<usize>, class: u8) -> LineToken {
        LineToken { range, class }
    }

    #[test]
    fn summarize_line_coalesces_and_breaks_on_whitespace() {
        // "ab cd" with one token over "ab" and one over "cd": a run per word,
        // broken by the space, each carrying its token's class.
        let runs = summarize_line("ab cd", &[tok(0..2, 1), tok(3..5, 2)], RowMark::NONE);
        assert_eq!(runs.as_slice(), [run(2, 2, 1), run(5, 2, 2)]);
    }

    #[test]
    fn summarize_line_coalesces_adjacent_same_class() {
        // Two tokens of the same class over contiguous chars merge into one run.
        let runs = summarize_line("abcd", &[tok(0..2, 1), tok(2..4, 1)], RowMark::NONE);
        assert_eq!(runs.as_slice(), [run(2, 4, 1)]);
    }

    #[test]
    fn summarize_line_uncovered_is_class_zero() {
        let runs = summarize_line("ab", &[], RowMark::NONE);
        assert_eq!(runs.as_slice(), [run(2, 2, 0)]);
    }

    #[test]
    fn summarize_line_expands_tabs_to_stops() {
        // A leading tab from the col-2 lane edge still advances to the next tab
        // stop at column 4, so the word starts at column 4.
        let runs = summarize_line("\tab", &[tok(1..3, 1)], RowMark::NONE);
        assert_eq!(runs.as_slice(), [run(4, 2, 1)]);
    }

    #[test]
    fn summarize_line_clamps_columns() {
        let line = "x".repeat(200);
        let runs = summarize_line(&line, &[], RowMark::NONE);
        assert_eq!(
            runs.as_slice(),
            [run(2, 118, 0)],
            "content fills the lane edge to the 120-column cap"
        );
    }

    #[test]
    fn summarize_line_last_run_swallows_overflow() {
        // Forty space-separated distinct-class chars would be forty runs, but
        // the last one absorbs everything from its start to the last char.
        const CHARS: usize = 40;
        let line: String = (0..CHARS).map(|_| "x ").collect();
        let tokens: Vec<LineToken> = (0..CHARS)
            .map(|i| tok(i * 2..i * 2 + 1, (i % 3 + 1) as u8))
            .collect();
        let runs = summarize_line(&line, &tokens, RowMark::NONE);
        assert_eq!(runs.len(), MAX_RUNS, "runs cap at the maximum");

        // An x sits at every even column, shifted past the 2-column lane, so
        // the last one ends at column 2 * CHARS + 1 and the last run reaches it.
        let last = runs.as_slice()[MAX_RUNS - 1];
        assert_eq!(
            (
                last.start_col as usize,
                last.start_col as usize + last.len as usize
            ),
            (2 * MAX_RUNS, 2 * CHARS + 1)
        );
    }

    /// A strip draws a line too small for letters, so the weight of the glyphs
    /// is what separates a line of code from a line of underscores. A run
    /// covers one weight, which is what puts that texture on the strip.
    #[test]
    fn summarize_line_splits_runs_on_glyph_weight() {
        // One class over the whole word, so only the weight breaks it: a
        // capital, then lowercase, then the punctuation after them.
        let runs = summarize_line("Abc;", &[tok(0..4, 1)], RowMark::NONE);
        assert_eq!(
            runs.as_slice(),
            [
                weighted(2, 1, 1, 255),
                weighted(3, 2, 1, 190),
                weighted(5, 1, 1, 110),
            ]
        );
    }

    /// An edit can insert rows past the build cursor, since the cursor sits
    /// wherever the last slice stopped. It summarizes those rows itself and the
    /// cursor advances by the same delta, so the summarized prefix and the
    /// claimed one stay equal and the build resumes at the seam.
    #[test]
    fn an_edit_reaching_past_the_build_cursor_summarizes_what_it_inserts() {
        // Two slices, so the sync that carries the edit also finishes the build.
        let total = 2 * SYNC_SLICE as usize;
        let before = rope(&vec!["line"; total].join("\n"));
        let mut content = MinimapContent::with_budget(1, test_executor(), Duration::ZERO);

        // One slice, leaving the cursor at SYNC_SLICE with the tail unbuilt.
        content.sync(
            &before,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        let _ = content.take_queued();
        assert_eq!(content.lines.len(), SYNC_SLICE as usize, "one slice built");

        // Three lines open on the cursor's last built row, so the rows they
        // occupy run past where the build had reached. Every line is "line\n", so
        // a row starts every five bytes.
        let seam = 5 * (SYNC_SLICE as usize - 1);
        let mut after: Vec<&str> = vec!["line"; total];
        after.splice(SYNC_SLICE as usize - 1..SYNC_SLICE as usize - 1, ["new"; 3]);
        let after = rope(&after.join("\n"));
        let edit = Patch::new(vec![Edit {
            old: seam..seam,
            new: seam..seam + 12,
        }]);
        content.sync(&after, 2, &edit, versions(0, 0), no_tokens, no_edges);

        // The same sync then builds the tail, so the two splices together show
        // the edit's reach and where the build picked up from it.
        assert_eq!(
            content
                .take_queued()
                .iter()
                .map(|s| (s.start, s.removed, s.lines.len()))
                .collect::<Vec<_>>(),
            vec![
                (SYNC_SLICE - 1, 1, 4),
                (SYNC_SLICE + 3, 0, total - SYNC_SLICE as usize),
            ],
            "the edit removes the one row it replaced, and the build resumes at \
             the row after the four it left, with no gap and no overlap",
        );
        assert_eq!(
            content.lines.len(),
            total + 3,
            "the store holds every row, the edit's included",
        );

        let inserted = summarize_line("new", &[], RowMark::NONE);
        assert_eq!(
            &content.lines[SYNC_SLICE as usize - 1..SYNC_SLICE as usize + 2],
            &[inserted, inserted, inserted][..],
            "including the rows that landed past where the build had reached",
        );
        assert!(!content.build_pending(), "and nothing is left to build");
    }

    /// An edit can also replace rows across the build cursor, and the store only
    /// held the ones before it. The splice has to report what the store gave up
    /// rather than what the buffer lost, or the terminal drops rows the strip
    /// never had.
    #[test]
    fn an_edit_replacing_across_the_build_cursor_reports_only_the_built_rows() {
        // Two slices, so the sync that carries the edit also finishes the build.
        let total = 2 * SYNC_SLICE as usize;
        let before = rope(&vec!["line"; total].join("\n"));
        let mut content = MinimapContent::with_budget(1, test_executor(), Duration::ZERO);

        content.sync(
            &before,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        let _ = content.take_queued();
        assert_eq!(content.lines.len(), SYNC_SLICE as usize, "one slice built");

        // Four rows collapse into one, starting two rows below the cursor, so two
        // of the four were built and two were not. Every line is "line\n", so a
        // row starts every five bytes and a row's last byte is its newline.
        let first = SYNC_SLICE as usize - 2;
        let after = rope(
            &[
                vec!["line"; first],
                vec!["ONE"],
                vec!["line"; total - first - 4],
            ]
            .concat()
            .join("\n"),
        );
        let edit = Patch::new(vec![Edit {
            old: 5 * first..5 * (first + 4) - 1,
            new: 5 * first..5 * first + 3,
        }]);
        content.sync(&after, 2, &edit, versions(0, 0), no_tokens, no_edges);

        let splices = content.take_queued();
        assert_eq!(
            (splices[0].start, splices[0].removed, splices[0].lines.len()),
            (first as u32, 2, 1),
            "the two built rows go, not the four the buffer lost",
        );
        assert_eq!(
            content.lines.len(),
            total - 3,
            "and the store still holds exactly the rows the cursor claims",
        );
    }

    /// A patch orders its old ranges in old-rope rows and its new ranges in
    /// new-rope rows, and its edits apply in order, so the rows under a later
    /// edit have already moved by the earlier ones' line delta. Indexing that
    /// edit by its old row would write it one delta too high.
    #[test]
    fn a_later_edit_in_one_patch_lands_on_the_moved_row() {
        let before = rope("alpha\nbravo\ncharlie\ndelta");
        let mut content = MinimapContent::new(1, test_executor());
        content.sync(
            &before,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        let _ = content.take_queued();

        // One patch that opens a line at the top and rewrites "charlie", whose
        // row the first edit pushes from 2 down to 3.
        let after = rope("new\nalpha\nbravo\nCHARLIE\ndelta");
        let edit = Patch::new(vec![
            Edit {
                old: 0..0,
                new: 0..4,
            },
            Edit {
                old: 12..19,
                new: 16..23,
            },
        ]);
        content.sync(&after, 2, &edit, versions(0, 0), no_tokens, no_edges);

        assert_eq!(
            content
                .take_queued()
                .iter()
                .map(|s| (s.start, s.removed, s.lines.len()))
                .collect::<Vec<_>>(),
            vec![(0, 1, 2), (3, 1, 1)],
            "the second edit splices the row it moved to, not the row it left",
        );
        assert_eq!(
            content.lines[3],
            summarize_line("CHARLIE", &[], RowMark::NONE),
            "and the store holds the rewritten line at that row",
        );
    }

    #[test]
    fn single_line_edit_queues_one_one_line_splice() {
        let before = rope("alpha\nbeta\ngamma\n");
        let mut content = MinimapContent::new(1, test_executor());
        content.sync(
            &before,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        content.take_queued();

        // Replace "beta" (line 1) in place.
        let after = rope("alpha\nBETA!\ngamma\n");
        let edit = Patch::new(vec![Edit {
            old: 6..10,
            new: 6..11,
        }]);
        content.sync(&after, 2, &edit, versions(0, 0), no_tokens, no_edges);

        let queued = content.take_queued();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].start, 1);
        assert_eq!(queued[0].removed, 1);
        assert_eq!(queued[0].lines.len(), 1, "one line re-summarized");
    }

    #[test]
    fn a_sync_queries_tokens_only_for_the_rows_it_touches() {
        use std::cell::RefCell;

        let queried: RefCell<Vec<Range<u32>>> = RefCell::new(Vec::new());
        let record = |rows: Range<u32>| -> HashMap<u32, Vec<LineToken>> {
            queried.borrow_mut().push(rows);
            HashMap::new()
        };

        let before = rope("a\nb\nc\nd\ne\nf\n");
        let mut content = MinimapContent::new(1, test_executor());

        // The initial build queries exactly the rows it fills.
        content.sync(
            &before,
            1,
            &Patch::empty(),
            versions(0, 0),
            record,
            no_edges,
        );
        assert_eq!(
            *queried.borrow(),
            vec![0..7],
            "the build queries only the rows it fills (6 lines and a trailing empty line)"
        );
        queried.borrow_mut().clear();
        content.take_queued();

        // An in-place edit on row 2 queries only that row, not the whole buffer.
        let after = rope("a\nb\nC\nd\ne\nf\n");
        let edit = Patch::new(vec![Edit {
            old: 4..5,
            new: 4..5,
        }]);
        content.sync(&after, 2, &edit, versions(0, 0), record, no_edges);
        assert_eq!(
            *queried.borrow(),
            vec![2..3],
            "an edit queries only its splice rows"
        );
    }

    #[test]
    fn newline_insertion_inserts_one_more_than_removed() {
        let before = rope("alpha\nbeta\ngamma\n");
        let mut content = MinimapContent::new(1, test_executor());
        content.sync(
            &before,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        content.take_queued();

        // Insert a newline inside "beta", splitting line 1 into two.
        let after = rope("alpha\nbe\nta\ngamma\n");
        let edit = Patch::new(vec![Edit {
            old: 8..8,
            new: 8..9,
        }]);
        content.sync(&after, 2, &edit, versions(0, 0), no_tokens, no_edges);

        let queued = content.take_queued();
        assert_eq!(queued.len(), 1);
        assert_eq!(
            queued[0].lines.len() as u32,
            queued[0].removed + 1,
            "inserted exceeds removed by one",
        );
    }

    /// `count` lines whose summaries differ from their neighbours', so a row
    /// that lands one place off reads as wrong.
    fn varied_lines(count: usize) -> Vec<String> {
        (0..count)
            .map(|row| format!("{} {row}", "ab".repeat(1 + row % 5)))
            .collect()
    }

    /// The byte offset each of `lines` starts at once joined with newlines.
    fn line_starts(lines: &[String]) -> Vec<usize> {
        lines
            .iter()
            .scan(0, |offset, line| {
                let start = *offset;
                *offset += line.len() + 1;
                Some(start)
            })
            .collect()
    }

    /// `text` inserted `column` bytes into each of the first `count` rows of
    /// every three, as one keystroke at that many cursors does.
    fn every_third_row(
        starts: &[usize],
        count: usize,
        column: usize,
        text: &'static str,
    ) -> Vec<(Range<usize>, &'static str)> {
        (0..count)
            .map(|i| {
                let at = starts[3 * i] + column;
                (at..at, text)
            })
            .collect()
    }

    /// `before` with each old range of `edits` replaced by its text, and the
    /// patch that makes the change. `edits` ascend and touch nowhere.
    fn patched(before: &str, edits: &[(Range<usize>, &str)]) -> (String, Patch<usize>) {
        let mut after = String::new();
        let mut patch = Vec::with_capacity(edits.len());
        let mut copied = 0;
        for (old, text) in edits {
            after.push_str(&before[copied..old.start]);
            let start = after.len();
            after.push_str(text);
            patch.push(Edit {
                old: old.clone(),
                new: start..after.len(),
            });
            copied = old.end;
        }
        after.push_str(&before[copied..]);
        (after, Patch::new(patch))
    }

    /// Apply `splices` to `store` in order, as the terminal applies them.
    fn replay(store: &mut Vec<LineRuns>, splices: Vec<Splice>) {
        for splice in splices {
            let start = splice.start as usize;
            store.splice(start..start + splice.removed as usize, splice.lines);
        }
    }

    /// Build `before` under `budget`, apply `edits` to it as one patch, and
    /// sync until [`MinimapContent::build_pending`] turns false.
    ///
    /// Returns the content, the terminal's store as the splices left it, and
    /// what a fresh build of the edited text holds.
    fn edit_and_settle(
        before: &str,
        budget: Duration,
        edits: &[(Range<usize>, &str)],
    ) -> (MinimapContent, Vec<LineRuns>, Vec<LineRuns>) {
        let tokens = color(3);
        let mut content = MinimapContent::with_budget(1, test_executor(), budget);
        let mut terminal = Vec::new();
        content.sync(
            &rope(before),
            1,
            &Patch::empty(),
            parse_versions(0, 1),
            &tokens,
            no_edges,
        );
        replay(&mut terminal, content.take_queued());

        let (after, patch) = patched(before, edits);
        let after = rope(&after);
        content.sync(&after, 2, &patch, parse_versions(0, 1), &tokens, no_edges);
        replay(&mut terminal, content.take_queued());
        while content.build_pending() {
            content.sync(
                &after,
                2,
                &Patch::empty(),
                parse_versions(0, 1),
                &tokens,
                no_edges,
            );
            replay(&mut terminal, content.take_queued());
        }

        let mut fresh = MinimapContent::with_budget(1, test_executor(), Duration::MAX);
        fresh.sync(
            &after,
            1,
            &Patch::empty(),
            parse_versions(0, 1),
            &tokens,
            no_edges,
        );
        (content, terminal, fresh.lines)
    }

    #[test]
    fn a_bulk_edit_ends_equal_to_a_fresh_build() {
        let lines = varied_lines(200);
        let edits = every_third_row(&line_starts(&lines), 64, 0, "z");

        let (content, terminal, fresh) = edit_and_settle(&lines.join("\n"), Duration::MAX, &edits);
        assert_eq!(
            content.lines, fresh,
            "the store ends where a fresh build does"
        );
        assert_eq!(terminal, fresh, "and so does the terminal");
    }

    #[test]
    fn a_bulk_newline_edit_keeps_every_row_aligned() {
        let lines = varied_lines(200);
        let edits = every_third_row(&line_starts(&lines), 64, 2, "\n");

        let (content, terminal, fresh) = edit_and_settle(&lines.join("\n"), Duration::MAX, &edits);
        assert_eq!(
            content.built_upto, 264,
            "every row is built, both halves of each split one included"
        );
        assert_eq!(
            content.lines, fresh,
            "the store ends where a fresh build does"
        );
        assert_eq!(terminal, fresh, "and so does the terminal");
    }

    #[test]
    fn a_bulk_edit_asks_one_slice_of_tokens_per_sync() {
        let lines = varied_lines(200);
        let before = lines.join("\n");
        let (after, patch) = patched(&before, &every_third_row(&line_starts(&lines), 64, 0, "z"));

        let queries = RefCell::new(0);
        let colored = color(3);
        let tokens = |rows: Range<u32>| {
            *queries.borrow_mut() += 1;
            colored(rows)
        };
        let mut content = MinimapContent::with_budget(1, test_executor(), Duration::ZERO);
        let before = rope(&before);
        loop {
            content.sync(
                &before,
                1,
                &Patch::empty(),
                parse_versions(0, 1),
                tokens,
                no_edges,
            );
            if !content.build_pending() {
                break;
            }
        }

        *queries.borrow_mut() = 0;
        content.sync(
            &rope(&after),
            2,
            &patch,
            parse_versions(0, 1),
            tokens,
            no_edges,
        );
        let asked = *queries.borrow();
        assert!(
            asked <= 1,
            "the sync asks for one slice of the sweep, not a query per edit, got {asked}"
        );
    }

    /// Two edits land on one row. The first joins an empty row onto the row
    /// below it, and the second adds a trailing space to the joined row, which
    /// keeps its row count and its summary.
    ///
    /// The joined row reads what the row below it read. A store that stood in
    /// with that summary while the terminal held the empty row's finds nothing
    /// to re-splice, and the terminal keeps the wrong row.
    #[test]
    fn a_bulk_edit_sharing_a_row_keeps_the_terminal_in_step() {
        let mut lines = varied_lines(200);
        lines[150] = String::new();
        lines[151] = "y".to_string();
        let starts = line_starts(&lines);
        let mut edits = every_third_row(&starts, 50, 0, "z");
        edits.push((starts[150]..starts[150] + 1, ""));
        edits.push((starts[151] + 1..starts[151] + 1, " "));

        let (content, terminal, fresh) = edit_and_settle(&lines.join("\n"), Duration::MAX, &edits);
        assert_eq!(
            content.lines, fresh,
            "the store ends where a fresh build does"
        );
        assert_eq!(terminal, fresh, "and so does the terminal");
    }

    /// A bulk patch whose last built edit replaces rows across the build
    /// cursor, followed by one past it. The store gives up only the rows it
    /// held, and the build fills the rest.
    #[test]
    fn a_bulk_edit_across_the_build_cursor_stays_aligned() {
        let lines = varied_lines(2 * SYNC_SLICE as usize);
        let starts = line_starts(&lines);
        let cursor = SYNC_SLICE as usize;
        let mut edits = every_third_row(&starts, 40, 0, "z");
        edits.push((starts[cursor - 2]..starts[cursor + 2] - 1, "ONE"));
        edits.push((starts[cursor + 100]..starts[cursor + 100], "z"));

        let (content, terminal, fresh) = edit_and_settle(&lines.join("\n"), Duration::ZERO, &edits);
        assert_eq!(
            content.lines, fresh,
            "the store ends where a fresh build does"
        );
        assert_eq!(terminal, fresh, "and so does the terminal");
    }

    #[test]
    fn multi_line_replace_removes_the_old_span() {
        let before = rope("a\nb\nc\nd\ne\n");
        let mut content = MinimapContent::new(1, test_executor());
        content.sync(
            &before,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        content.take_queued();

        // Replace lines 1..=3 ("b\nc\nd") with one line "X".
        let after = rope("a\nX\ne\n");
        let edit = Patch::new(vec![Edit {
            old: 2..7,
            new: 2..3,
        }]);
        content.sync(&after, 2, &edit, versions(0, 0), no_tokens, no_edges);

        let queued = content.take_queued();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].start, 1);
        assert_eq!(queued[0].removed, 3, "three old lines removed");
        assert_eq!(queued[0].lines.len(), 1, "one new line inserted");
    }

    #[test]
    fn sliced_build_appends_until_complete() {
        let total = SYNC_SLICE + SYNC_SLICE / 2;
        // Exactly `total` lines, with no trailing newline that would add an empty
        // last line.
        let text: String = vec!["line"; total as usize].join("\n");
        let rope = rope(&text);
        let mut content = MinimapContent::with_budget(1, test_executor(), Duration::ZERO);

        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        let first = content.take_queued();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].start, 0);
        assert_eq!(
            first[0].lines.len() as u32,
            SYNC_SLICE,
            "first slice is full"
        );
        assert!(
            content.build_pending(),
            "the build still has a slice after the first sync"
        );

        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        let second = content.take_queued();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].start, SYNC_SLICE);
        assert_eq!(
            second[0].lines.len() as u32,
            SYNC_SLICE / 2,
            "the remainder finishes the build",
        );
        assert!(
            !content.build_pending(),
            "the build is complete after the last slice"
        );

        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        assert!(content.take_queued().is_empty(), "nothing left to build");
    }

    /// Every stage stops on the clock rather than on a row count, so a budget
    /// wide enough carries a file of several slices in one sync.
    ///
    /// A slice is the step between two clock readings, never a cap on what one
    /// sync covers. A stage that stopped at the first slice would leave a file
    /// of plain lines waiting a frame per slice for no reason.
    #[test]
    fn a_wide_budget_carries_every_stage_past_one_slice() {
        let total = SYNC_SLICE + SYNC_SLICE / 2;
        let rope = rope(&vec!["line"; total as usize].join("\n"));
        let mut content = MinimapContent::with_budget(1, test_executor(), Duration::MAX);

        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        content.take_queued();
        assert_eq!(
            content.lines.len() as u32,
            total,
            "the build covers every row in one sync"
        );

        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 1),
            color(1),
            no_edges,
        );
        assert_eq!(
            spliced_rows(&content.take_queued()).len() as u32,
            total,
            "and the recolor sweep re-summarizes every row in one sync"
        );

        let marked = [1u32, SYNC_SLICE + 1];
        let asked = RefCell::new(Vec::new());
        let marks = RecordingEdges {
            marked: &marked,
            asked: &asked,
        };
        content.sync(&rope, 1, &Patch::empty(), versions(1, 1), no_tokens, marks);
        assert_eq!(
            spliced_rows(&content.take_queued()),
            vec![1, SYNC_SLICE + 1],
            "and the edge sweep reaches a mark in the second slice"
        );

        assert!(!content.build_pending(), "with no stage left owing rows");
    }

    /// A sync measures its budget on the executor's clock. On a still clock, the
    /// budget never ends a sync. A clock that moves past the budget during a
    /// slice stops the sync at the end of that slice.
    #[test]
    fn the_budget_runs_on_the_executor_clock() {
        let scheduler = Arc::new(TestScheduler::new());
        let rope = rope(&vec!["line"; 2 * SYNC_SLICE as usize].join("\n"));
        let built_in_one_sync = |step: Duration| {
            let mut content = MinimapContent::new(1, scheduler.executor());
            let tokens = |_: Range<u32>| {
                scheduler.advance_clock(step);
                HashMap::new()
            };
            content.sync(&rope, 1, &Patch::empty(), versions(0, 0), tokens, no_edges);
            content.lines.len() as u32
        };

        assert_eq!(
            (
                built_in_one_sync(Duration::ZERO),
                built_in_one_sync(SYNC_BUDGET)
            ),
            (2 * SYNC_SLICE, SYNC_SLICE),
            "rows one sync builds on a still clock and on a clock past the budget"
        );
    }

    /// A monochrome strip of a slice and a half, at a budget too small to reach
    /// a second slice, so every stage of a later sync covers exactly one.
    fn built_recolor_fixture() -> (Rope, MinimapContent) {
        let total = SYNC_SLICE + SYNC_SLICE / 2;
        let text: String = vec!["line"; total as usize].join("\n");
        let rope = rope(&text);
        let mut content = MinimapContent::with_budget(1, test_executor(), Duration::ZERO);
        // Build the whole file monochrome over two syncs (syntax 0 sweeps nothing).
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        content.take_queued();
        (rope, content)
    }

    fn color(class: u8) -> impl Fn(Range<u32>) -> HashMap<u32, Vec<LineToken>> {
        move |rows: Range<u32>| rows.map(|r| (r, vec![tok(0..4, class)])).collect()
    }

    /// [`color`] in `base`, with `row` alone in `class`.
    fn color_except(
        base: u8,
        row: u32,
        class: u8,
    ) -> impl Fn(Range<u32>) -> HashMap<u32, Vec<LineToken>> {
        move |rows: Range<u32>| {
            rows.map(|r| (r, vec![tok(0..4, if r == row { class } else { base })]))
                .collect()
        }
    }

    /// A monochrome strip short enough to build and sweep in one sync, so a
    /// scoped sweep's row set is exactly what the splices report.
    ///
    /// Left settled at syntax version 1 with nothing pending, since a fresh
    /// content starts out owing a full sweep it has not yet been asked for.
    fn small_recolor_fixture(lines: u32) -> (Rope, MinimapContent) {
        let rope = rope(&vec!["line"; lines as usize].join("\n"));
        let mut content = MinimapContent::new(1, test_executor());
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            parse_versions(0, 1),
            no_tokens,
            no_edges,
        );
        content.take_queued();
        (rope, content)
    }

    /// The rows a run of splices re-summarized, flattened out of the coalescing.
    fn spliced_rows(splices: &[Splice]) -> Vec<u32> {
        splices
            .iter()
            .flat_map(|s| s.start..s.start + s.lines.len() as u32)
            .collect()
    }

    #[test]
    fn a_parse_sweeps_only_the_rows_it_reported() {
        let (rope, mut content) = small_recolor_fixture(100);

        content.note_syntax_rows(Some(10..12));
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            parse_versions(0, 2),
            color(1),
            no_edges,
        );

        assert_eq!(
            spliced_rows(&content.take_queued()),
            vec![10, 11],
            "a parse reporting two rows must not restain the other 98",
        );
        assert!(
            !content.build_pending(),
            "the scoped sweep finished, so nothing is outstanding"
        );
    }

    #[test]
    fn a_parse_reporting_no_rows_queues_nothing() {
        let (rope, mut content) = small_recolor_fixture(100);

        content.note_syntax_rows(Some(0..0));
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            parse_versions(0, 2),
            color(1),
            no_edges,
        );
        assert!(
            content.take_queued().is_empty(),
            "a parse whose tokens are unchanged must not restain anything",
        );

        // The version still advanced, so a later sync does not re-enter the sweep.
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            parse_versions(0, 2),
            color(1),
            no_edges,
        );
        assert!(
            content.take_queued().is_empty(),
            "the skipped sweep still advanced the synced version",
        );
    }

    #[test]
    fn a_parse_reporting_no_rows_at_all_sweeps_everything() {
        let (rope, mut content) = small_recolor_fixture(20);

        content.note_syntax_rows(None);
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            parse_versions(0, 2),
            color(1),
            no_edges,
        );

        assert_eq!(
            spliced_rows(&content.take_queued()),
            (0..20).collect::<Vec<_>>(),
            "an unreported parse leaves every row suspect",
        );
    }

    #[test]
    fn a_non_parse_bump_sweeps_everything_despite_a_reported_span() {
        let (rope, mut content) = small_recolor_fixture(20);

        // The highlight toggle and LSP tokens carry no row information, so they
        // must widen the sweep past whatever the last parse reported.
        content.note_syntax_rows(Some(10..12));
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 2),
            color(1),
            no_edges,
        );

        assert_eq!(
            spliced_rows(&content.take_queued()),
            (0..20).collect::<Vec<_>>(),
            "a bump with no row information behind it restains every row",
        );
    }

    #[test]
    fn reports_between_syncs_accumulate() {
        let (rope, mut content) = small_recolor_fixture(100);

        content.note_syntax_rows(Some(10..12));
        content.note_syntax_rows(Some(40..41));
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            parse_versions(0, 2),
            color(1),
            no_edges,
        );

        assert_eq!(
            spliced_rows(&content.take_queued()),
            (10..41).collect::<Vec<_>>(),
            "two parses landing between syncs are covered by the hull of their rows",
        );
    }

    #[test]
    fn an_edit_above_a_reported_span_slides_it_with_the_rows() {
        let (_before, mut content) = small_recolor_fixture(100);

        // A parse reports rows 40..42, then four lines are deleted above them
        // before the sync that acts on the report. The span names rows in the
        // pre-edit rope, so it has to slide with them or the sweep restains the
        // wrong lines and leaves the reported ones stale.
        content.note_syntax_rows(Some(40..42));
        let after = rope(&vec!["line"; 96].join("\n"));
        let edit = Patch::new(vec![Edit {
            old: 0..20,
            new: 0..0,
        }]);
        content.sync(&after, 2, &edit, parse_versions(0, 2), color(1), no_edges);

        let mut swept = spliced_rows(&content.take_queued());
        // The edit's own rows re-summarize on the buffer-edit path, so drop
        // what it queued and look at what the sweep added.
        swept.retain(|row| *row >= 4);
        assert_eq!(
            swept,
            vec![36, 37],
            "the reported rows slid up by the four deleted lines",
        );
    }

    /// The rows a full sweep already passed carry version 1's colors, which
    /// differ from version 2's only on the row the second parse reports. So the
    /// sweep keeps its cursor, and takes that row once the cursor is done.
    #[test]
    fn a_bump_landing_mid_sweep_resweeps_only_its_rows_outside_the_cursor() {
        let total = SYNC_SLICE + SYNC_SLICE / 2;
        let (rope, mut content) = built_recolor_fixture();

        // Park a full sweep's cursor at SYNC_SLICE.
        content.note_syntax_rows(None);
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            parse_versions(0, 1),
            color(1),
            no_edges,
        );
        content.take_queued();

        content.note_syntax_rows(Some(100..101));
        let mut swept = Vec::new();
        for _ in 0..8 {
            content.sync(
                &rope,
                1,
                &Patch::empty(),
                parse_versions(0, 2),
                color_except(1, 100, 2),
                no_edges,
            );
            swept.extend(spliced_rows(&content.take_queued()));
            if !content.build_pending() {
                break;
            }
        }

        assert_eq!(
            swept,
            (SYNC_SLICE..total).chain([100]).collect::<Vec<_>>(),
            "the cursor finishes its window, then the reported row behind it",
        );
        let expected: Vec<LineRuns> = (0..total)
            .map(|row| {
                let class = if row == 100 { 2 } else { 1 };
                summarize_line("line", &[tok(0..4, class)], RowMark::NONE)
            })
            .collect();
        assert_eq!(content.lines, expected);
    }

    /// A parse that lands on every sync of a full sweep adds its row for the
    /// end. The cursor keeps its place, so the sweep finishes while the parses
    /// land.
    #[test]
    fn a_sweep_finishes_while_a_parse_lands_every_sync() {
        let (rope, mut content) = built_recolor_fixture();
        content.note_syntax_rows(None);
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            parse_versions(0, 1),
            color(1),
            no_edges,
        );

        for version in 2..6 {
            if !content.build_pending() {
                break;
            }
            content.note_syntax_rows(Some(10..11));
            content.sync(
                &rope,
                1,
                &Patch::empty(),
                parse_versions(0, version),
                color_except(1, 10, (version % 2 + 1) as u8),
                no_edges,
            );
        }

        assert!(
            !content.build_pending(),
            "the sweep finished within four more syncs"
        );
    }

    /// A report past the end of a scoped sweep's window joins the sweep too,
    /// since the cursor never reaches it.
    #[test]
    fn a_report_past_a_scoped_sweep_joins_it() {
        let (rope, mut content) = built_recolor_fixture();
        // Settle a full sweep to version 1 first, so the next report scopes one.
        content.note_syntax_rows(None);
        for _ in 0..2 {
            content.sync(
                &rope,
                1,
                &Patch::empty(),
                parse_versions(0, 1),
                color(1),
                no_edges,
            );
        }
        content.note_syntax_rows(Some(0..600));
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            parse_versions(0, 2),
            color(2),
            no_edges,
        );
        content.take_queued();

        content.note_syntax_rows(Some(700..701));
        let mut swept = Vec::new();
        for _ in 0..8 {
            content.sync(
                &rope,
                1,
                &Patch::empty(),
                parse_versions(0, 3),
                color_except(2, 700, 3),
                no_edges,
            );
            swept.extend(spliced_rows(&content.take_queued()));
            if !content.build_pending() {
                break;
            }
        }

        assert_eq!(swept, (SYNC_SLICE..600).chain([700]).collect::<Vec<_>>());
    }

    /// An edit above the rows held for the end slides them with the text. The
    /// sweep then restains the row the parse reported, not its old index.
    #[test]
    fn an_edit_slides_the_rows_held_for_the_end() {
        let total = SYNC_SLICE + SYNC_SLICE / 2;
        let (before, mut content) = built_recolor_fixture();
        content.note_syntax_rows(None);
        content.sync(
            &before,
            1,
            &Patch::empty(),
            parse_versions(0, 1),
            color(1),
            no_edges,
        );
        // The cursor finishes its window and holds row 100 for the next sync.
        content.note_syntax_rows(Some(100..101));
        content.sync(
            &before,
            1,
            &Patch::empty(),
            parse_versions(0, 2),
            color_except(1, 100, 2),
            no_edges,
        );
        content.take_queued();

        // Four lines deleted at the top move row 100 to row 96.
        let after = rope(&vec!["line"; total as usize - 4].join("\n"));
        let edit = Patch::new(vec![Edit {
            old: 0..20,
            new: 0..0,
        }]);
        content.sync(
            &after,
            2,
            &edit,
            parse_versions(0, 2),
            color_except(1, 96, 2),
            no_edges,
        );

        let mut swept = spliced_rows(&content.take_queued());
        // The edit's own rows re-summarize on the buffer-edit path.
        swept.retain(|row| *row >= 4);
        assert_eq!(swept, vec![96], "the held row slid up by the four lines");
    }

    #[test]
    fn a_recolor_sweeps_in_slices_across_syncs() {
        let total = SYNC_SLICE + SYNC_SLICE / 2;
        let (rope, mut content) = built_recolor_fixture();

        // A syntax bump recolors, one slice per sync.
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 1),
            color(1),
            no_edges,
        );
        let first = content.take_queued();
        assert_eq!(
            first.len(),
            1,
            "the slice's changed rows are contiguous, so they ship as one splice"
        );
        assert_eq!(first[0].start, 0);
        assert_eq!(
            (first[0].removed, first[0].lines.len()),
            (SYNC_SLICE, SYNC_SLICE as usize),
            "and that splice still covers the whole slice"
        );
        assert_eq!(
            first[0].lines[0],
            summarize_line("line", &[tok(0..4, 1)], RowMark::NONE),
            "the swept summary matches a direct resummarize"
        );

        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 1),
            color(1),
            no_edges,
        );
        let second = content.take_queued();
        assert_eq!(second.len(), 1, "the remainder ships as one splice too");
        assert_eq!(
            second[0].lines.len(),
            (total - SYNC_SLICE) as usize,
            "the remainder finishes the sweep"
        );
        assert_eq!(second[0].start, SYNC_SLICE);

        // The sweep has reached the version, so a further sync recolors nothing.
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 1),
            color(1),
            no_edges,
        );
        assert!(
            content.take_queued().is_empty(),
            "a settled recolor queues nothing"
        );
    }

    /// A parse that reports no rows leaves every line suspect. If it lands
    /// mid-sweep, the sweep restarts at the top against the new target.
    #[test]
    fn an_unreported_bump_mid_sweep_restarts_the_sweep() {
        let (rope, mut content) = built_recolor_fixture();

        // The first recolor sweeps slice 0 to class 1, leaving the cursor mid-file.
        content.note_syntax_rows(None);
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            parse_versions(0, 1),
            color(1),
            no_edges,
        );
        content.take_queued();

        content.note_syntax_rows(None);
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            parse_versions(0, 2),
            color(2),
            no_edges,
        );
        let restarted = content.take_queued();
        assert_eq!(
            restarted[0].start, 0,
            "an unreported bump restarts the sweep at the top"
        );
        assert_eq!(restarted.len(), 1, "the restarted slice ships coalesced");
        assert_eq!(restarted[0].lines.len(), SYNC_SLICE as usize);
        assert_eq!(
            restarted[0].lines[0],
            summarize_line("line", &[tok(0..4, 2)], RowMark::NONE)
        );
    }

    #[test]
    fn class_table_palette_has_one_entry_per_class() {
        use super::ClassTable;
        use crate::{display_map::syntax_theme::SyntaxStyles, theme::Theme};

        let table = ClassTable::from_theme(&Theme::empty());
        let scopes = SyntaxStyles::from_theme(&Theme::empty()).theme_keys().len();
        assert_eq!(
            table.palette().len(),
            5 * (scopes + 1) + 7,
            "a syntax block of the default foreground and one color per scope, the \
             seven edge classes, then one copy of the syntax block per population",
        );
    }

    #[test]
    fn summarize_line_prepends_the_edge_lane() {
        // An edge fills the reserved cols 0-1 and content starts at col 2.
        let runs = summarize_line(
            "ab",
            &[tok(0..2, 1)],
            RowMark {
                edge: Some(40),
                class_base: 0,
            },
        );
        assert_eq!(runs.as_slice(), [weighted(0, 2, 40, 255), run(2, 2, 1)]);
    }

    #[test]
    fn summarize_line_offsets_content_classes_by_the_row_base() {
        let mark = RowMark {
            edge: Some(34),
            class_base: 39,
        };
        let runs = summarize_line("ab cd", &[tok(0..2, 1), tok(3..5, 2)], mark);
        assert_eq!(
            runs.as_slice(),
            [weighted(0, 2, 34, 255), run(2, 2, 40), run(5, 2, 41)],
            "the content classes take the base, and the lane keeps its edge class",
        );
    }

    #[test]
    fn edge_class_appends_after_the_syntax_scopes() {
        use super::{ClassTable, EdgeClass};
        use crate::{display_map::syntax_theme::SyntaxStyles, theme::Theme};

        let table = ClassTable::from_theme(&Theme::empty());
        let base = table.edge_class(EdgeClass::Added);
        assert_eq!(table.edge_class(EdgeClass::Removed), base + 1);
        assert_eq!(table.edge_class(EdgeClass::Info), base + 5);

        let scopes = SyntaxStyles::from_theme(&Theme::empty()).theme_keys().len();
        assert_eq!(
            base as usize,
            scopes + 1,
            "the base follows the syntax palette"
        );
    }

    /// The population blocks follow the edge classes, each a copy of the syntax
    /// block, and the palette an older terminal reads is a prefix.
    #[test]
    fn status_blocks_follow_the_edge_classes() {
        use super::{ClassTable, EdgeClass};
        use crate::{host::DiffStatus, theme::Theme};

        let theme = {
            let (config, errors) = stoat_config::parse(
                "theme t { syntax.keyword.fg = blue; syntax.string.fg = green; }",
            );
            assert!(errors.is_empty(), "parse errors: {errors:?}");
            Theme::from_config(&config.expect("the theme parses"), "t").expect("theme t exists")
        };
        let table = ClassTable::from_theme(&theme);
        let stride = table.stride();
        let header = table.edge_class(EdgeClass::Header);
        assert_eq!(
            [
                table.population_base(DiffStatus::Unchanged),
                table.population_base(DiffStatus::Added),
                table.population_base(DiffStatus::Modified),
                table.population_base(DiffStatus::Moved),
                table.deleted_base(),
            ],
            [
                0,
                header + 1,
                header + 1 + stride,
                header + 1 + 2 * stride,
                header + 1 + 3 * stride,
            ],
        );

        let palette = table.palette();
        let syntax = &palette[..stride as usize];
        let block = |base: u8| &palette[base as usize..(base + stride) as usize];
        assert_ne!(
            syntax[1], syntax[3],
            "the syntax block holds distinct colors"
        );
        assert_eq!(
            [
                block(table.population_base(DiffStatus::Added)),
                block(table.population_base(DiffStatus::Modified)),
                block(table.population_base(DiffStatus::Moved)),
                block(table.deleted_base()),
            ],
            [syntax; 4],
            "each population block copies the syntax block",
        );
        assert_eq!(
            (
                palette.len(),
                table.legacy_palette(),
                table.edge_class(EdgeClass::Added)
            ),
            (
                5 * stride as usize + 7,
                &palette[..header as usize + 1],
                stride
            ),
        );
    }

    #[test]
    fn build_carries_each_line_edge() {
        let text = rope("alpha\nbravo");
        let mut content = MinimapContent::new(1, test_executor());
        let edge_of = |row: u32| (row == 1).then_some(40);

        content.sync(
            &text,
            1,
            &Patch::empty(),
            versions(5, 0),
            no_tokens,
            edge_of,
        );

        let built = &content.take_queued()[0].lines;
        assert_eq!(
            built[0].as_slice()[0],
            run(2, 5, 0),
            "an unmarked line starts past the lane"
        );
        assert_eq!(
            built[1].as_slice()[0],
            weighted(0, 2, 40, 255),
            "a marked line leads with its edge"
        );
    }

    /// A row whose diff status moves under an unchanged edge re-summarizes, so
    /// its content takes the new block while its lane reads the same.
    #[test]
    fn a_base_change_under_a_steady_edge_resplices_the_row() {
        struct BasedEdges<'a>(&'a std::cell::Cell<u8>);

        impl EdgeSource for BasedEdges<'_> {
            fn mark_of(&self, row: u32) -> RowMark {
                match row {
                    1 => RowMark {
                        edge: Some(40),
                        class_base: self.0.get(),
                    },
                    _ => RowMark::NONE,
                }
            }
        }

        let text = rope("alpha\nbravo");
        let tokens = |rows: Range<u32>| -> HashMap<u32, Vec<LineToken>> {
            rows.map(|row| (row, vec![tok(0..5, 1)])).collect()
        };
        let base = std::cell::Cell::new(0);
        let mut content = MinimapContent::new(1, test_executor());
        content.sync(
            &text,
            1,
            &Patch::empty(),
            versions(5, 0),
            tokens,
            BasedEdges(&base),
        );
        content.take_queued();

        base.set(39);
        content.sync(
            &text,
            1,
            &Patch::empty(),
            versions(6, 0),
            tokens,
            BasedEdges(&base),
        );
        let line = summarize_line(
            "bravo",
            &[tok(0..5, 1)],
            RowMark {
                edge: Some(40),
                class_base: 39,
            },
        );
        assert_eq!(
            (content.take_queued(), line.as_slice()),
            (
                vec![Splice {
                    start: 1,
                    removed: 1,
                    lines: vec![line],
                }],
                [weighted(0, 2, 40, 255), run(2, 5, 40)].as_slice()
            ),
            "row 1 alone resplices, and its content run takes class 40",
        );
    }

    /// The frame loop only ticks more syncs while `build_pending` holds, so a
    /// sweep that reports false after its first slice strands every row past it
    /// with stale colors.
    #[test]
    fn an_in_flight_sweep_keeps_reporting_pending() {
        let (rope, mut content) = built_recolor_fixture();
        assert!(
            !content.build_pending(),
            "the fixture is fully built, so nothing is pending before the bump"
        );

        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 1),
            color(1),
            no_edges,
        );
        let _ = content.take_queued();
        assert!(
            content.build_pending(),
            "the sweep has rows left, so idle ticks must keep coming"
        );

        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 1),
            color(1),
            no_edges,
        );
        let _ = content.take_queued();
        assert!(
            !content.build_pending(),
            "the finished sweep lets the frame loop go idle again"
        );
    }

    /// The sweep cursor is an absolute row, so a deletion above it slides
    /// unswept rows underneath it. A cursor left in place resumes past those
    /// rows and strands them on their pre-recolor runs.
    #[test]
    fn an_edit_above_the_sweep_cursor_moves_it_with_the_rows() {
        let total = SYNC_SLICE + SYNC_SLICE / 2;
        let (before, mut content) = built_recolor_fixture();

        // Sweep the first slice, parking the cursor at SYNC_SLICE.
        content.sync(
            &before,
            1,
            &Patch::empty(),
            versions(0, 1),
            color(1),
            no_edges,
        );
        content.take_queued();

        // Delete the first four lines, well above the cursor, so every unswept
        // row slides four rows up.
        let after = rope(&vec!["line"; total as usize - 4].join("\n"));
        let edit = Patch::new(vec![Edit {
            old: 0..20,
            new: 0..0,
        }]);
        content.sync(&after, 2, &edit, versions(0, 1), color(1), no_edges);

        while content.build_pending() {
            content.sync(
                &after,
                2,
                &Patch::empty(),
                versions(0, 1),
                color(1),
                no_edges,
            );
        }

        let recolored = summarize_line("line", &[tok(0..4, 1)], RowMark::NONE);
        let stale: Vec<usize> = content
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| **line != recolored)
            .map(|(row, _)| row)
            .collect();

        assert_eq!(
            content.lines.len(),
            total as usize - 4,
            "the edit shortened the strip by four rows"
        );
        assert!(
            stale.is_empty(),
            "the finished sweep left rows {stale:?} on their pre-recolor runs"
        );
    }

    /// The slide reads the rows the edit replaced as the store held them, which a
    /// prior edit in the same patch has already moved. Reading them unshifted
    /// puts a cursor sitting just above the later edit on the wrong side of it,
    /// and it jumps forward over rows neither edit re-summarized.
    #[test]
    fn a_later_edit_in_one_patch_leaves_the_sweep_cursor_put() {
        let total = SYNC_SLICE + SYNC_SLICE / 2;
        let (before, mut content) = built_recolor_fixture();

        // Sweep the first slice, parking the cursor at SYNC_SLICE.
        content.sync(
            &before,
            1,
            &Patch::empty(),
            versions(0, 1),
            color(1),
            no_edges,
        );
        content.take_queued();

        // Five lines open at the top, sliding the cursor to SYNC_SLICE + 5, and
        // one more opens two rows below where the cursor lands. Every line is
        // "line\n", so a row starts every five bytes.
        let after = rope(&vec!["line"; total as usize + 6].join("\n"));
        let second = 5 * (SYNC_SLICE as usize + 2);
        let edit = Patch::new(vec![
            Edit {
                old: 0..0,
                new: 0..25,
            },
            Edit {
                old: second..second,
                new: second + 25..second + 30,
            },
        ]);
        content.sync(&after, 2, &edit, versions(0, 1), color(1), no_edges);

        while content.build_pending() {
            content.sync(
                &after,
                2,
                &Patch::empty(),
                versions(0, 1),
                color(1),
                no_edges,
            );
        }

        let recolored = summarize_line("line", &[tok(0..4, 1)], RowMark::NONE);
        let stale: Vec<usize> = content
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| **line != recolored)
            .map(|(row, _)| row)
            .collect();

        assert!(
            stale.is_empty(),
            "the sweep skipped rows {stale:?}, which neither edit re-summarized"
        );
    }

    /// Coalescing must not paper over a gap. An unchanged row between two
    /// changed ones has to break the run, or the merged splice would overwrite
    /// it with whatever fell either side.
    #[test]
    fn an_unchanged_row_breaks_the_coalesced_run() {
        let total = 6u32;
        let text: String = vec!["line"; total as usize].join("\n");
        let rope = rope(&text);
        let mut content = MinimapContent::new(1, test_executor());
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        let _ = content.take_queued();

        // Recolor every row but 3, so the sweep changes 0-2 and 4-5.
        let tokens = |rows: Range<u32>| {
            rows.filter(|row| *row != 3)
                .map(|row| (row, vec![tok(0..4, 1)]))
                .collect::<HashMap<_, _>>()
        };
        content.sync(&rope, 1, &Patch::empty(), versions(0, 1), tokens, no_edges);

        let splices = content.take_queued();
        assert_eq!(splices.len(), 2, "the untouched row splits the run");
        assert_eq!((splices[0].start, splices[0].lines.len()), (0, 3));
        assert_eq!((splices[1].start, splices[1].lines.len()), (4, 2));
    }

    #[test]
    fn an_edge_sweep_coalesces_each_contiguous_run() {
        let total = 10u32;
        let text: String = vec!["line"; total as usize].join("\n");
        let rope = rope(&text);
        let mut content = MinimapContent::new(1, test_executor());
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        let _ = content.take_queued();

        // Marks appear on rows 5, 6, and 9: one adjacent pair and a loner.
        let edge_of = |row: u32| matches!(row, 5 | 6 | 9).then_some(40);
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(1, 0),
            no_tokens,
            edge_of,
        );

        let splices = content.take_queued();
        assert_eq!(splices.len(), 2, "5-6 merge, 9 stands alone");
        assert_eq!((splices[0].start, splices[0].lines.len()), (5, 2));
        assert_eq!((splices[1].start, splices[1].lines.len()), (9, 1));
    }

    /// Switching branches moves a mark on every changed row at once, so the
    /// edge sweep is sliced the way the recolor sweep is. Each sync looks only
    /// at its own window, and the marks past it wait their turn.
    #[test]
    fn an_edge_sweep_covers_a_slice_per_sync() {
        let total = SYNC_SLICE + SYNC_SLICE / 2;
        let (rope, mut content) = built_recolor_fixture();

        // A mark in each slice, so a sweep that stopped early is visible.
        let marked = [1u32, SYNC_SLICE + 1];
        let asked = RefCell::new(Vec::new());
        let marks = RecordingEdges {
            marked: &marked,
            asked: &asked,
        };

        content.sync(&rope, 1, &Patch::empty(), versions(1, 0), no_tokens, marks);
        assert!(
            asked.borrow().iter().all(|&row| row < SYNC_SLICE),
            "the first sync stays inside its own window",
        );
        let first = content.take_queued();
        assert_eq!(first.len(), 1, "only the first slice's mark landed");
        assert_eq!(first[0].start, 1);

        asked.borrow_mut().clear();
        content.sync(&rope, 1, &Patch::empty(), versions(1, 0), no_tokens, marks);
        assert!(
            asked.borrow().iter().all(|&row| row >= SYNC_SLICE),
            "and the second picks up where the first stopped",
        );
        let second = content.take_queued();
        assert_eq!(second.len(), 1, "the second slice's mark lands next");
        assert_eq!(second[0].start, SYNC_SLICE + 1);

        // The sweep has reached the end, so a third sync has nothing left.
        asked.borrow_mut().clear();
        content.sync(&rope, 1, &Patch::empty(), versions(1, 0), no_tokens, marks);
        assert!(
            asked.borrow().is_empty(),
            "a settled sweep re-checks nothing",
        );
        assert!(content.take_queued().is_empty());
        assert_eq!(total, SYNC_SLICE + SYNC_SLICE / 2, "two slices of rows");
    }

    /// An edge sweep with rows left keeps the frame loop ticking, the way the
    /// recolor sweep does.
    ///
    /// The caller syncs on an idle frame only while the content says work is
    /// pending. A sweep that stops saying so covers one slice per unrelated
    /// user event, which on a large file leaves most rows carrying the marks
    /// of the branch that was checked out before.
    #[test]
    fn an_in_flight_edge_sweep_keeps_reporting_pending() {
        let (rope, mut content) = built_recolor_fixture();
        assert!(
            !content.build_pending(),
            "the fixture is fully built, so nothing is pending before the bump"
        );

        let marked = [1u32, SYNC_SLICE + 1];
        let asked = RefCell::new(Vec::new());
        let marks = RecordingEdges {
            marked: &marked,
            asked: &asked,
        };

        content.sync(&rope, 1, &Patch::empty(), versions(1, 0), no_tokens, marks);
        let _ = content.take_queued();
        assert!(
            content.build_pending(),
            "the sweep has rows left, so idle ticks must keep coming"
        );

        content.sync(&rope, 1, &Patch::empty(), versions(1, 0), no_tokens, marks);
        let _ = content.take_queued();
        assert!(
            !content.build_pending(),
            "the finished sweep lets the frame loop go idle again"
        );
    }

    /// Past a handful of moved marks the window's slice of the edge list is
    /// rebuilt in one pass rather than poked an entry at a time. The rebuild
    /// has to land on exactly the marks the source reports, including where a
    /// row gains one next to a row losing one.
    #[test]
    fn a_batched_edge_apply_lands_on_the_marks_the_source_reports() {
        let rows = 40u32;
        let text: String = vec!["line"; rows as usize].join("\n");
        let rope = rope(&text);
        let mut content = MinimapContent::new(1, test_executor());
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        content.take_queued();

        // Every third row, then every fourth. Some rows keep their mark, some
        // lose it, some gain one, interleaved the whole way down, and both
        // changes are far past the batch threshold.
        let marks_of = |f: fn(u32) -> Option<u8>| -> Vec<(u32, RowMark)> {
            (0..rows)
                .filter_map(|row| {
                    f(row).map(|c| {
                        (
                            row,
                            RowMark {
                                edge: Some(c),
                                class_base: 0,
                            },
                        )
                    })
                })
                .collect()
        };
        fn every_third(row: u32) -> Option<u8> {
            row.is_multiple_of(3).then_some(40)
        }
        fn every_fourth(row: u32) -> Option<u8> {
            row.is_multiple_of(4).then_some(41)
        }

        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(1, 0),
            no_tokens,
            every_third,
        );
        assert_eq!(content.edges, marks_of(every_third));

        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(2, 0),
            no_tokens,
            every_fourth,
        );
        assert_eq!(
            content.edges,
            marks_of(every_fourth),
            "the rebuilt slice holds the new marks and nothing of the old",
        );

        // One row moving takes the per-row path, which has to agree.
        fn every_fourth_but_eight(row: u32) -> Option<u8> {
            (row.is_multiple_of(4) && row != 8).then_some(41)
        }
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(3, 0),
            no_tokens,
            every_fourth_but_eight,
        );
        assert_eq!(content.edges, marks_of(every_fourth_but_eight));
    }

    /// A decoration bump runs on every diff recompute and diagnostic batch, and
    /// usually moves no mark, so it must not resolve tokens for the whole built
    /// range just to discover there is nothing to do.
    #[test]
    fn an_edge_resync_that_changes_nothing_fetches_no_tokens() {
        let text = rope("alpha\nbravo\ncharlie");
        let mut content = MinimapContent::new(1, test_executor());
        content.sync(
            &text,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        let _ = content.take_queued();

        let fetches = std::cell::Cell::new(0);
        let counting = |rows: Range<u32>| {
            fetches.set(fetches.get() + 1);
            no_tokens(rows)
        };
        content.sync(
            &text,
            1,
            &Patch::empty(),
            versions(1, 0),
            counting,
            no_edges,
        );

        assert_eq!(fetches.get(), 0, "no mark moved, so no tokens are resolved");
        assert!(content.take_queued().is_empty());
    }

    /// Every branch that reads the marks is gated, and the caller resolves its
    /// diff hunks on the first row asked about. A branch that reached for a
    /// mark unconditionally would put that resolve back on every frame while
    /// changing nothing it emits, so the silence is what has to be pinned.
    #[test]
    fn a_sync_with_nothing_to_do_consults_no_edge_source() {
        let text = rope("alpha\nbravo\ncharlie");
        let mut content = MinimapContent::new(1, test_executor());
        content.sync(
            &text,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        let _ = content.take_queued();

        content.sync(
            &text,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            UnusedEdges,
        );

        assert!(
            content.take_queued().is_empty(),
            "nothing moved, so nothing splices"
        );
    }

    #[test]
    fn an_edge_resync_fetches_only_the_changed_rows() {
        let text = rope("alpha\nbravo\ncharlie\ndelta\necho");
        let mut content = MinimapContent::new(1, test_executor());
        content.sync(
            &text,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        let _ = content.take_queued();

        let spans = RefCell::new(Vec::new());
        let counting = |rows: Range<u32>| {
            spans.borrow_mut().push(rows.clone());
            no_tokens(rows)
        };
        // Marks land on rows 1, 2, and 4: one adjacent pair and a loner.
        let edge_of = |row: u32| matches!(row, 1 | 2 | 4).then_some(40);
        content.sync(&text, 1, &Patch::empty(), versions(1, 0), counting, edge_of);

        assert_eq!(
            *spans.borrow(),
            vec![1..3, 4..5],
            "one fetch per contiguous run of changed rows, never the whole file",
        );
        let queued = content.take_queued();
        assert_eq!(
            queued.iter().map(|s| s.start).collect::<Vec<_>>(),
            vec![1, 4],
            "1 and 2 coalesce into one splice, 4 stands alone",
        );
    }

    /// Resolving one row's mark seeks the diff hunks, so a bump on a large file
    /// must not ask about rows no mark can reach.
    #[test]
    fn an_edge_resync_asks_only_about_markable_rows() {
        let total = 4000u32;
        let text = rope(&vec!["line"; total as usize].join("\n"));
        let mut content = MinimapContent::new(1, test_executor());

        let asked = RefCell::new(Vec::new());
        let marked = [1000u32, 1001, 1002];
        let source = RecordingEdges {
            marked: &marked,
            asked: &asked,
        };

        content.sync(&text, 1, &Patch::empty(), versions(0, 0), no_tokens, source);
        let _ = content.take_queued();
        asked.borrow_mut().clear();

        content.sync(&text, 1, &Patch::empty(), versions(1, 0), no_tokens, source);

        assert_eq!(
            *asked.borrow(),
            vec![1000, 1001, 1002],
            "the markable rows, not the {total} built ones",
        );
        assert!(
            content.take_queued().is_empty(),
            "no mark moved, so nothing splices"
        );
    }

    /// An edit above a marked row moves that row, and the marks are keyed by
    /// row, so they have to move with it or the next bump re-splices two lines
    /// to put a mark back where it already was.
    #[test]
    fn an_edit_slides_the_marks_below_it() {
        let before = rope("alpha\nbravo\ncharlie\ndelta");
        let mut content = MinimapContent::new(1, test_executor());
        let asked = RefCell::new(Vec::new());

        content.sync(
            &before,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            RecordingEdges {
                marked: &[3],
                asked: &asked,
            },
        );
        let _ = content.take_queued();

        // Open a line above everything, sliding the marked row from 3 to 4.
        let after = rope("new\nalpha\nbravo\ncharlie\ndelta");
        let edit = Patch::new(vec![Edit {
            old: 0..0,
            new: 0..4,
        }]);
        content.sync(
            &after,
            2,
            &edit,
            versions(0, 0),
            no_tokens,
            RecordingEdges {
                marked: &[4],
                asked: &asked,
            },
        );
        let _ = content.take_queued();
        asked.borrow_mut().clear();

        content.sync(
            &after,
            2,
            &Patch::empty(),
            versions(1, 0),
            no_tokens,
            RecordingEdges {
                marked: &[4],
                asked: &asked,
            },
        );

        assert_eq!(*asked.borrow(), vec![4], "row 3 no longer holds the mark");
        assert!(
            content.take_queued().is_empty(),
            "the mark already sits on row 4, so nothing re-splices"
        );
    }

    /// An edit that deletes a marked row has to take its mark with it, or the
    /// row sliding up into its place inherits a mark it never had.
    #[test]
    fn an_edit_drops_the_marks_of_the_rows_it_deletes() {
        let before = rope("alpha\nbravo\ncharlie");
        let mut content = MinimapContent::new(1, test_executor());
        let asked = RefCell::new(Vec::new());

        content.sync(
            &before,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            RecordingEdges {
                marked: &[1],
                asked: &asked,
            },
        );
        let _ = content.take_queued();

        let after = rope("alpha\ncharlie");
        let edit = Patch::new(vec![Edit {
            old: 6..12,
            new: 6..6,
        }]);
        let unmarked = RecordingEdges {
            marked: &[],
            asked: &asked,
        };
        content.sync(&after, 2, &edit, versions(0, 0), no_tokens, unmarked);
        let _ = content.take_queued();
        asked.borrow_mut().clear();

        content.sync(
            &after,
            2,
            &Patch::empty(),
            versions(1, 0),
            no_tokens,
            unmarked,
        );

        assert!(
            asked.borrow().is_empty(),
            "the deleted row took its mark with it, got {:?}",
            asked.borrow(),
        );
        assert!(content.take_queued().is_empty(), "nothing left to unmark");
    }

    /// The rows that can carry a mark do not cover the rows that already do, so
    /// a mark the source stops reporting would otherwise stay painted forever.
    #[test]
    fn a_mark_the_source_drops_is_cleared() {
        let text = rope("alpha\nbravo\ncharlie");
        let mut content = MinimapContent::new(1, test_executor());
        let asked = RefCell::new(Vec::new());

        content.sync(
            &text,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            RecordingEdges {
                marked: &[1],
                asked: &asked,
            },
        );
        let _ = content.take_queued();
        asked.borrow_mut().clear();

        content.sync(
            &text,
            1,
            &Patch::empty(),
            versions(1, 0),
            no_tokens,
            RecordingEdges {
                marked: &[],
                asked: &asked,
            },
        );

        assert_eq!(*asked.borrow(), vec![1], "the row still holding a mark");
        let queued = content.take_queued();
        assert_eq!(
            queued
                .iter()
                .map(|s| (s.start, s.removed))
                .collect::<Vec<_>>(),
            vec![(1, 1)],
        );
        assert_eq!(
            queued[0].lines[0],
            summarize_line("bravo", &[], RowMark::NONE),
            "the line re-summarizes without its lane",
        );
    }

    #[test]
    fn decoration_change_splices_only_the_marked_line() {
        let text = rope("alpha\nbravo\ncharlie");
        let mut content = MinimapContent::new(1, test_executor());

        content.sync(
            &text,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        let _ = content.take_queued();

        // The buffer is unchanged, but a diagnostic appears on line 1.
        let edge_of = |row: u32| (row == 1).then_some(40);
        content.sync(
            &text,
            1,
            &Patch::empty(),
            versions(1, 0),
            no_tokens,
            edge_of,
        );

        let splices = content.take_queued();
        assert_eq!(splices.len(), 1, "only the newly marked line splices");
        assert_eq!(splices[0].start, 1);
        assert_eq!(splices[0].removed, 1);
        assert_eq!(
            splices[0].lines[0].as_slice()[0],
            weighted(0, 2, 40, 255),
            "the mark leads the line"
        );
    }

    #[test]
    fn syntax_change_resummarizes_content() {
        let text = rope("alpha\nbeta");
        let mut content = MinimapContent::new(1, test_executor());

        // Build with line 0 colored class 5 across the whole word.
        let colored = |rows: Range<u32>| {
            let mut map = HashMap::new();
            if rows.contains(&0) {
                map.insert(0, vec![tok(0.."alpha".len(), 5)]);
            }
            map
        };
        content.sync(&text, 1, &Patch::empty(), versions(0, 1), colored, no_edges);
        let _ = content.take_queued();

        // The buffer is unchanged, but the syntax version bumps and the color is
        // gone, so line 0 re-summarizes monochrome and line 1 stays put.
        content.sync(
            &text,
            1,
            &Patch::empty(),
            versions(0, 2),
            no_tokens,
            no_edges,
        );

        let splices = content.take_queued();
        assert_eq!(splices.len(), 1, "only the recolored line splices");
        assert_eq!(splices[0].start, 0);
        assert_eq!(
            splices[0].lines[0].as_slice(),
            [run(2, 5, 0)],
            "line 0 goes monochrome"
        );
    }

    /// A recolor cannot move a mark, so the sweep takes each row's from the
    /// store rather than paying a seek per line to resolve it again. The rows
    /// still have to come out marked, since reading nothing would silently
    /// strip the lane off every line a recolor touched.
    #[test]
    fn a_recolor_sweep_keeps_the_marks_without_asking_for_them() {
        let text = rope("alpha\nbravo");
        let mut content = MinimapContent::new(1, test_executor());
        let asked = RefCell::new(Vec::new());
        let source = RecordingEdges {
            marked: &[1],
            asked: &asked,
        };

        content.sync(&text, 1, &Patch::empty(), versions(0, 1), no_tokens, source);
        let _ = content.take_queued();
        asked.borrow_mut().clear();

        // The buffer and the marks stand still, only the coloring moves.
        content.sync(&text, 1, &Patch::empty(), versions(0, 2), color(2), source);

        assert!(
            asked.borrow().is_empty(),
            "a recolor resolves no marks, got {:?}",
            asked.borrow(),
        );

        let splices = content.take_queued();
        assert_eq!(
            splices.iter().map(|s| s.start).collect::<Vec<_>>(),
            vec![0],
            "both rows recolor, so they coalesce into one splice",
        );
        assert_eq!(
            splices[0].lines,
            vec![
                summarize_line("alpha", &[tok(0..4, 2)], RowMark::NONE),
                summarize_line(
                    "bravo",
                    &[tok(0..4, 2)],
                    RowMark {
                        edge: Some(40),
                        class_base: 0
                    }
                ),
            ],
            "the marked row keeps its lane through the recolor",
        );
    }

    /// Scopes keep distinct classes even when they paint the same color.
    ///
    /// An unstyled theme gives every scope the same fallback foreground. A
    /// color-keyed lookup cannot tell those apart, and would collapse every
    /// scope onto whichever one it saw first.
    #[test]
    fn class_of_maps_a_style_id_to_its_scope_class() {
        use super::ClassTable;
        use crate::{display_map::syntax_theme::SyntaxStyles, theme::Theme};
        use stoat_language::HighlightId;

        let theme = Theme::empty();
        let table = ClassTable::from_theme(&theme);
        let styles = SyntaxStyles::from_theme(&theme);
        assert_eq!(
            table.palette()[1],
            table.palette()[2],
            "the two scopes do paint identically, so only the id separates them",
        );

        let first = styles
            .id_for_highlight(HighlightId(0))
            .expect("the first theme key resolves");
        assert_eq!(table.class_of(first), 1, "the i-th scope takes class i + 1",);
        assert_eq!(
            table.class_of(styles.id_for_highlight(HighlightId(1)).expect("second key")),
            2,
        );
    }

    /// A span with a resolved color maps to the class of the first key that
    /// paints in that color. The text color, a color no key paints in, and no
    /// color map to class 0.
    #[test]
    fn class_of_fg_maps_a_resolved_color_to_its_first_key_class() {
        use super::ClassTable;
        use crate::theme::Theme;
        use ratatui::style::Color;

        let src = r##"theme fgtest {
            ui.text.fg = "#ffffff";
            syntax.keyword.fg = "#ff0000";
            syntax.string.fg = "#00ff00";
            syntax.comment.fg = "#00ff00";
            syntax.function.fg = "#ffffff";
        }"##;
        let (config, _) = stoat_config::parse(src);
        let theme = Theme::from_config(&config.expect("theme config parses"), "fgtest")
            .expect("theme builds");
        let table = ClassTable::from_theme(&theme);

        let rgb = |r, g, b| Some(Color::Rgb(r, g, b));
        assert_eq!(
            [
                rgb(255, 0, 0),
                rgb(0, 255, 0),
                rgb(255, 255, 255),
                rgb(1, 2, 3),
                None
            ]
            .map(|fg| table.class_of_fg(fg)),
            [1, 3, 0, 0, 0],
        );
    }

    #[test]
    fn oversized_buffer_disables_and_queues_nothing() {
        let text: String = (0..MAX_LINES + 1).map(|_| "x\n").collect();
        let rope = rope(&text);
        let mut content = MinimapContent::new(1, test_executor());

        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );

        assert!(
            content.take_queued().is_empty(),
            "a huge buffer queues nothing"
        );
        assert_eq!(
            content.take_queued(),
            Vec::<Splice>::new(),
            "and stays disabled on further syncs",
        );
    }
}
