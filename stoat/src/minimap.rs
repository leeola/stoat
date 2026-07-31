//! Pure per-line run summaries feeding the stoatty minimap strip.
//!
//! A [`MinimapContent`] holds one summary per buffer line, updated incrementally
//! from buffer edits and filled progressively for a freshly opened file, and
//! drains the changes as [`Splice`]s the emission layer turns into `minimap_lines`
//! frames. [`summarize_line`] compresses a line into colored [`Run`] blocks, never
//! the file text. Nothing here does IO, so the whole module is unit-tested.

use crate::{
    display_map::{highlights::HighlightStyleId, syntax_theme::SyntaxStyles},
    theme::{scope, Theme},
};
use ratatui::style::Color;
use std::{collections::HashMap, ops::Range};
use stoat_language::HighlightId;
use stoat_text::{
    patch::{Edit, Patch},
    Rope,
};

/// Minimap columns a line is summarized into, matching the strip's declared
/// `max_columns`. Display columns past this are dropped.
const MAX_COLUMNS: u32 = 120;

/// Display width of a tab stop.
const TAB_WIDTH: u32 = 4;

/// Runs kept per line. The last run swallows any overflow to end-of-line, so a
/// busy line never emits an unbounded number of blocks.
const MAX_RUNS: usize = 12;

/// Lines summarized per [`MinimapContent::sync`] during the initial build, so a
/// large file fills over several frames rather than stalling one.
const BUILD_CHUNK: u32 = 4096;

/// Lines re-summarized per [`MinimapContent::sync`] during a recolor sweep, so a
/// large file's syntax recolor spreads across frames rather than stalling one.
pub(crate) const RESYNC_CHUNK: u32 = 4096;

/// Line count past which a buffer disables its minimap, so a huge file neither
/// summarizes nor emits.
const MAX_LINES: usize = 500_000;

/// Buffer lines drawn per vertical strip cell, matching the strip's declared
/// `lines_per_cell`. A pointer row therefore spans this many lines.
pub const LINES_PER_CELL: u32 = 8;

/// Columns reserved on a line's left edge for the diff/diagnostic mark lane, so
/// a mark never overwrites the code silhouette. Content starts after it.
const LANE_WIDTH: u32 = 2;

/// A single colored run on one line, `len` display columns from `start_col`
/// drawn in palette class `class`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Run {
    pub start_col: u8,
    pub len: u8,
    pub class: u8,
}

/// A line's diff or diagnostic state, drawn as a colored run in the reserved
/// left-edge lane.
///
/// The six kinds occupy palette classes appended after the syntax scopes,
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
    pub lines: Vec<Vec<Run>>,
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
/// Any `Fn(u32) -> Option<u8>` is an edge source, answering the bulk question
/// with the conservative every-row answer.
pub trait EdgeSource {
    /// The edge-lane class of `row`, or `None` when it carries no mark.
    fn edge_of(&self, row: u32) -> Option<u8>;

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
    fn edge_of(&self, row: u32) -> Option<u8> {
        self(row)
    }
}

/// The run summaries of one buffer, plus the incremental-sync bookkeeping.
///
/// Mirrors the terminal's content store: one entry per line, spliced as the
/// buffer changes. Retains the rope it last synced against so an edit's old byte
/// range resolves to old rows, and a build cursor so a fresh file fills a chunk
/// at a time.
pub struct MinimapContent {
    content_id: u32,
    lines: Vec<Vec<Run>>,
    /// The built rows carrying a mark and the edge-lane class each carries,
    /// ascending by row. A row absent from it is unmarked.
    ///
    /// Sparse rather than one entry per built line so it doubles as the index of
    /// what is marked. A diff or diagnostic change re-summarizes the rows whose
    /// mark moved, and finding those means visiting what is marked now together
    /// with what [`EdgeSource::marked_rows`] says could become marked, neither
    /// of which is a walk of the file.
    edges: Vec<(u32, u8)>,
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
    /// [`Self::pending_syntax_rows`]' start, and it advances [`RESYNC_CHUNK`]
    /// rows per sync until it reaches [`Self::resync_end`].
    resync_upto: Option<u32>,
    /// The row an active sweep stops at, or `None` to run to
    /// [`Self::built_upto`]. Always clamped to `built_upto` in use.
    resync_end: Option<u32>,
    /// The syntax version an active sweep is bringing the strip to, so a fresh
    /// bump to a different version restarts the sweep at the top.
    resync_target: u64,
    /// Rows reported changed since the last sweep finished, awaiting the next
    /// sweep. Reset when a sweep consumes it.
    pending_syntax_rows: SweepRows,
    queued: Vec<Splice>,
}

impl MinimapContent {
    pub fn new(content_id: u32) -> MinimapContent {
        MinimapContent {
            content_id,
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
            resync_target: 0,
            pending_syntax_rows: SweepRows::All,
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

    /// Whether chunked work is still outstanding, from either the initial build
    /// or a recolor sweep.
    ///
    /// The caller ticks [`Self::sync`] on idle frames while this holds, so both
    /// cursors run to completion instead of stalling until the next user event.
    /// The sweep needs this as much as the build does. It advances one
    /// [`RESYNC_CHUNK`] per sync on a file that is already fully built, so
    /// without it every row past the first chunk keeps stale colors until some
    /// unrelated event happens to tick another sync.
    pub fn build_pending(&self) -> bool {
        !self.disabled
            && (self.built_upto < line_count(&self.synced_rope) || self.resync_upto.is_some())
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
    /// Edits within the already-built prefix queue splices. The unbuilt tail fills
    /// up to [`BUILD_CHUNK`] lines per call. A buffer over [`MAX_LINES`] disables
    /// and queues nothing.
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
            for edit in edits.edits() {
                self.apply_edit(edit, new_rope, &tokens_for, &marks);
            }
            self.synced_version = version;
            self.synced_rope = new_rope.clone();
        }

        if self.built_upto < total {
            let end = (self.built_upto + BUILD_CHUNK).min(total);
            let tokens = tokens_for(self.built_upto..end);
            let lines = summarize_rows(new_rope, self.built_upto..end, &tokens, &marks);
            self.queue_splice(Splice {
                start: self.built_upto,
                removed: 0,
                lines: lines.clone(),
            });
            self.edges
                .extend((self.built_upto..end).filter_map(|row| Some((row, marks.edge_of(row)?))));
            self.lines.extend(lines);
            self.built_upto = end;
        }

        // A change with no row information behind it can have restained any
        // line, so it widens whatever the parses have reported to everything.
        if versions.syntax_other != self.synced_syntax_other {
            self.pending_syntax_rows = SweepRows::All;
            self.synced_syntax_other = versions.syntax_other;
        }

        // A recolor re-summarizes the built lines' content, swept RESYNC_CHUNK
        // rows per sync so a large file never recolors in one frame. A syntax
        // version change starts the sweep over the rows reported changed since
        // the last one finished. A fresh bump to a new version mid-sweep
        // restarts it against the new target.
        if versions.syntax != self.synced_syntax_version
            && (self.resync_upto.is_none() || self.resync_target != versions.syntax)
        {
            // The rows an in-flight sweep already covered carry the old
            // target's colors, so its remaining range and the new one cannot
            // simply be unioned. Cover everything instead.
            let rows = match self.resync_upto {
                Some(_) => SweepRows::All,
                None => std::mem::replace(&mut self.pending_syntax_rows, SweepRows::NONE),
            };
            self.resync_target = versions.syntax;
            match rows {
                SweepRows::All => {
                    self.resync_upto = Some(0);
                    self.resync_end = None;
                },
                // Nothing changed, so the strip is already at the new version.
                SweepRows::Rows(rows) if rows.is_empty() => {
                    self.resync_upto = None;
                    self.synced_syntax_version = versions.syntax;
                },
                SweepRows::Rows(rows) => {
                    self.resync_upto = Some(rows.start);
                    self.resync_end = Some(rows.end);
                },
            }
        }
        if let Some(from) = self.resync_upto {
            let end = self
                .resync_end
                .unwrap_or(self.built_upto)
                .min(self.built_upto);
            let from = from.min(end);
            let to = from.saturating_add(RESYNC_CHUNK).min(end);
            self.resync_chunk(new_rope, from..to, &tokens_for);
            if to >= end {
                self.resync_upto = None;
                self.synced_syntax_version = self.resync_target;
            } else {
                self.resync_upto = Some(to);
            }
        }

        // A diff or diagnostic change re-checks every built line's edge mark,
        // independent of the content sweep. The sweep above paints the marks as
        // of the last decoration sync, so a row whose mark this bump moves is
        // re-spliced here even when the sweep just touched it.
        if versions.decoration != self.synced_decoration_version {
            self.resync_edges(new_rope, &tokens_for, &marks);
            self.synced_decoration_version = versions.decoration;
        }
    }

    /// Re-summarize the built lines in `range` and queue a one-line splice where
    /// a line's full summary changed, for a recolor (highlight toggle or a
    /// completed parse) that leaves the buffer text untouched.
    ///
    /// A recolor can touch any line, so the caller sweeps the whole built range
    /// one [`RESYNC_CHUNK`] at a time across successive syncs rather than in one.
    ///
    /// Each row keeps the mark it already carries, read from the stored index
    /// rather than resolved again. A recolor cannot move a mark, and asking the
    /// source would cost a seek per line of the file for an answer it already
    /// holds. A decoration bump landing in the same sync makes those marks one
    /// version stale, which [`Self::resync_edges`] corrects immediately after.
    fn resync_chunk(
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
            let summary = summarize_line(&text, row_tokens(&tokens, row), self.edge_at(row));
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
    ) {
        let suspect = {
            let mut rows = marks.marked_rows(0..self.built_upto);
            rows.extend(self.edges.iter().map(|&(row, _)| row));
            rows.retain(|&row| row < self.built_upto);
            rows.sort_unstable();
            rows.dedup();
            rows
        };

        let changed: Vec<(u32, Option<u8>)> = suspect
            .into_iter()
            .filter_map(|row| {
                let new_edge = marks.edge_of(row);
                (self.edge_at(row) != new_edge).then_some((row, new_edge))
            })
            .collect();
        if changed.is_empty() {
            return;
        }

        let mut text = String::new();
        for run in contiguous_runs(&changed) {
            let rows = changed[run].iter();
            let span = rows.clone().next().expect("a run is non-empty").0
                ..rows.clone().next_back().expect("a run is non-empty").0 + 1;
            let tokens = tokens_for(span.clone());

            // A run's rows are consecutive, so one walk over its span advances
            // in step with them.
            let mut walk = new_rope.line_walk(span);
            for &(row, new_edge) in rows {
                text.clear();
                walk.next_into(&mut text);
                let summary = summarize_line(&text, row_tokens(&tokens, row), new_edge);
                self.set_edge(row, new_edge);
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
        let inserted_edges: Vec<Option<u8>> = (new_start_row..new_end_row + 1)
            .map(|row| marks.edge_of(row))
            .collect();

        let start = new_start_row as usize;
        let end = (start + removed as usize).min(self.lines.len());
        self.lines.splice(start..end, inserted.iter().cloned());
        self.splice_edges(new_start_row..end as u32, &inserted_edges);

        let delta = inserted.len() as i64 - removed as i64;
        self.built_upto = (self.built_upto as i64 + delta).max(0) as u32;

        // Sweep bounds are absolute rows, so rows the edit slid underneath them
        // would never be swept and would keep their pre-recolor runs. Rows the
        // edit re-summarized already carry current tokens, so resuming at the
        // seam of a straddling edit re-sweeps them idempotently.
        //
        // `resync_end` of `None` runs to `built_upto`, which the splice above
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
        }
        if let SweepRows::Rows(rows) = &self.pending_syntax_rows
            && !rows.is_empty()
        {
            self.pending_syntax_rows = SweepRows::Rows(slide(rows.start)..slide(rows.end));
        }

        self.queue_splice(Splice {
            start: new_start_row,
            removed,
            lines: inserted,
        });
    }

    /// Store `summary` as `row`'s and queue the one-line splice carrying it.
    ///
    /// The summary `row` held supplies the splice's payload allocation, since it
    /// is dead the moment the new one replaces it. The store and the payload
    /// remain two owned copies of the runs. The wire replay consumes the
    /// payload, while the store stays to answer the next sync's comparison.
    fn replace_line(&mut self, row: u32, summary: Vec<Run>) {
        let mut payload = std::mem::replace(&mut self.lines[row as usize], summary);
        payload.clone_from(&self.lines[row as usize]);

        self.queue_splice(Splice {
            start: row,
            removed: 1,
            lines: vec![payload],
        });
    }

    /// The edge-lane class `row` carries, or `None` when it is unmarked.
    fn edge_at(&self, row: u32) -> Option<u8> {
        let at = self.edges.binary_search_by_key(&row, |&(r, _)| r).ok()?;
        Some(self.edges[at].1)
    }

    /// Record that `row` now carries `edge`, dropping it from the marked rows
    /// when that is `None`.
    fn set_edge(&mut self, row: u32, edge: Option<u8>) {
        let at = self.edges.binary_search_by_key(&row, |&(r, _)| r);
        match (at, edge) {
            (Ok(at), Some(class)) => self.edges[at].1 = class,
            (Ok(at), None) => {
                self.edges.remove(at);
            },
            (Err(at), Some(class)) => self.edges.insert(at, (row, class)),
            (Err(_), None) => {},
        }
    }

    /// Replace the marks of `rows` with `inserted`'s, one entry per row from
    /// `rows.start`, sliding the rows after `rows` by the line delta.
    ///
    /// Mirrors the splice [`Self::lines`] takes for the same edit, so the two
    /// keep agreeing on which row is which.
    fn splice_edges(&mut self, rows: Range<u32>, inserted: &[Option<u8>]) {
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
                .filter_map(|(i, edge)| Some((rows.start + i as u32, (*edge)?))),
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
fn contiguous_runs(changed: &[(u32, Option<u8>)]) -> Vec<Range<usize>> {
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

/// Summarize each row in `rows`, its tokens from `tokens` and its edge mark from
/// `marks`.
fn summarize_rows(
    rope: &Rope,
    rows: Range<u32>,
    tokens: &HashMap<u32, Vec<LineToken>>,
    marks: &impl EdgeSource,
) -> Vec<Vec<Run>> {
    let mut text = String::new();
    let mut walk = rope.line_walk(rows.clone());
    rows.map(|row| {
        // A row the walk has run out for summarizes as empty, which is what
        // reading it a row at a time produced. Skipping it instead would shift
        // every later summary onto the wrong row.
        text.clear();
        walk.next_into(&mut text);
        summarize_line(&text, row_tokens(tokens, row), marks.edge_of(row))
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
/// When `edge` is set, a run fills the reserved [`LANE_WIDTH`]-column lane at the
/// left in that class, and the content summary starts after it so the mark never
/// overwrites the code silhouette.
///
/// Content walks the line by display column, a tab advancing to the next multiple
/// of [`TAB_WIDTH`] and other chars advancing one, capped at [`MAX_COLUMNS`]. A
/// non-whitespace char extends the current run when it is contiguous and shares
/// the covering token's class, otherwise opens a new run. Whitespace ends the
/// current run, so a gap breaks the blocks. A char uncovered by any token is
/// class 0. Once [`MAX_RUNS`] runs exist, the last run swallows the rest of the
/// line.
pub fn summarize_line(line: &str, tokens: &[LineToken], edge: Option<u8>) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    if let Some(class) = edge {
        runs.push(Run {
            start_col: 0,
            len: LANE_WIDTH as u8,
            class,
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
            let class = match tokens.get(token_idx) {
                Some(token) if token.range.start <= byte => token.class,
                _ => 0,
            };

            let contiguous = runs.last().is_some_and(|last| {
                last.class == class && last.start_col as u32 + last.len as u32 == col
            });
            if overflowed || contiguous {
                let last = runs.last_mut().expect("a run to extend");
                last.len = (col + width - last.start_col as u32) as u8;
            } else if runs.len() < MAX_RUNS {
                runs.push(Run {
                    start_col: col as u8,
                    len: width as u8,
                    class,
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

/// The first buffer line the strip renders, tracking the editor's viewport.
///
/// A strip that can show `visible_lines` lines slides its window across the file
/// in proportion to how far the viewport (`view_top` over the scrollable
/// `total - view_visible` span) has scrolled, mapping the whole file onto the
/// strip. Returns 0 when the file fits the strip.
pub fn minimap_top(total: f32, visible_lines: f32, view_top: f32, view_visible: f32) -> f32 {
    if total <= visible_lines {
        return 0.0;
    }
    let scrollable = total - view_visible;
    let ratio = if scrollable > 0.0 {
        (view_top / scrollable).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ratio * (total - visible_lines)
}

/// The buffer line a pointer at strip cell-row `row` (0-based from the strip top)
/// points at, for a strip `strip_rows` cells tall over the given viewport.
///
/// The strip shows `strip_rows * LINES_PER_CELL` lines from [`minimap_top`], and
/// each cell spans [`LINES_PER_CELL`] lines, so the click lands on that cell's
/// middle line. A row past the strip clamps to its last cell.
pub fn click_target_line(
    strip_rows: u16,
    row: u16,
    total: f32,
    view_top: f32,
    view_visible: f32,
) -> u32 {
    let lines_per_cell = LINES_PER_CELL as f32;
    let visible_lines = strip_rows as f32 * lines_per_cell;
    let top = minimap_top(total, visible_lines, view_top, view_visible);
    let row = row.min(strip_rows.saturating_sub(1)) as f32;
    (top + row * lines_per_cell + lines_per_cell / 2.0).max(0.0) as u32
}

/// Maps a buffer's syntax highlight styles to compact minimap classes and the
/// palette they draw in.
///
/// Class 0 is the theme's default foreground for uncovered text. Class `i + 1` is
/// the resolved foreground of the `i`-th syntax scope. The emission layer declares
/// [`Self::palette`] on the strip, and a token's [`HighlightStyleId`] resolves to
/// its class via [`Self::class_of`].
pub struct ClassTable {
    palette: Vec<[u8; 3]>,
    by_style: HashMap<HighlightStyleId, u8>,
    edge_base: u8,
}

impl ClassTable {
    pub fn from_theme(theme: &Theme) -> ClassTable {
        let styles = SyntaxStyles::from_theme(theme);
        let default_fg = color_to_rgb(theme.get("ui.text").fg.unwrap_or(Color::White));

        let mut palette = vec![default_fg];
        let mut by_style = HashMap::new();
        for index in 0..styles.theme_keys().len() {
            let Some(style_id) = styles.id_for_highlight(HighlightId(index as u32)) else {
                palette.push(default_fg);
                continue;
            };
            let class = (index + 1) as u8;
            by_style.insert(style_id, class);
            let fg = styles.interner[style_id].foreground.unwrap_or(Color::White);
            palette.push(color_to_rgb(fg));
        }

        // The six edge classes follow the syntax scopes, in EdgeClass order, so a
        // run's class indexes its mark color directly.
        let edge_base = palette.len() as u8;
        for scope in [
            scope::DIFF_ADDED,
            scope::DIFF_DELETED,
            scope::DIFF_MODIFIED,
            scope::UI_DIAGNOSTIC_ERROR,
            scope::UI_DIAGNOSTIC_WARNING,
            scope::UI_DIAGNOSTIC_INFO,
        ] {
            palette.push(color_to_rgb(theme.get(scope).fg.unwrap_or(Color::White)));
        }

        ClassTable {
            palette,
            by_style,
            edge_base,
        }
    }

    /// The palette class the edge mark `kind` draws in.
    ///
    /// The six edge classes occupy the palette right after the syntax scopes, in
    /// [`EdgeClass`] order, so the returned class indexes the mark's declared
    /// color on the strip.
    pub fn edge_class(&self, kind: EdgeClass) -> u8 {
        self.edge_base + kind as u8
    }

    /// The class a token drawn in `style` maps to, or 0 when the style is not a
    /// recognized syntax scope.
    pub fn class_of(&self, style: HighlightStyleId) -> u8 {
        self.by_style.get(&style).copied().unwrap_or(0)
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
        summarize_line, EdgeSource, LineToken, MinimapContent, Run, Splice, SyncVersions,
        BUILD_CHUNK, MAX_LINES, RESYNC_CHUNK,
    };
    use std::{cell::RefCell, collections::HashMap, ops::Range};
    use stoat_text::{
        patch::{Edit, Patch},
        Rope,
    };

    fn rope(text: &str) -> Rope {
        Rope::from(text)
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
        fn edge_of(&self, row: u32) -> Option<u8> {
            self.asked.borrow_mut().push(row);
            self.marked.contains(&row).then_some(40)
        }

        fn marked_rows(&self, rows: Range<u32>) -> Vec<u32> {
            self.marked
                .iter()
                .copied()
                .filter(|row| rows.contains(row))
                .collect()
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
        Run {
            start_col,
            len,
            class,
        }
    }

    fn tok(range: Range<usize>, class: u8) -> LineToken {
        LineToken { range, class }
    }

    #[test]
    fn summarize_line_coalesces_and_breaks_on_whitespace() {
        // "ab cd" with one token over "ab" and one over "cd": a run per word,
        // broken by the space, each carrying its token's class.
        let runs = summarize_line("ab cd", &[tok(0..2, 1), tok(3..5, 2)], None);
        assert_eq!(runs, vec![run(2, 2, 1), run(5, 2, 2)]);
    }

    #[test]
    fn summarize_line_coalesces_adjacent_same_class() {
        // Two tokens of the same class over contiguous chars merge into one run.
        let runs = summarize_line("abcd", &[tok(0..2, 1), tok(2..4, 1)], None);
        assert_eq!(runs, vec![run(2, 4, 1)]);
    }

    #[test]
    fn summarize_line_uncovered_is_class_zero() {
        let runs = summarize_line("ab", &[], None);
        assert_eq!(runs, vec![run(2, 2, 0)]);
    }

    #[test]
    fn summarize_line_expands_tabs_to_stops() {
        // A leading tab from the col-2 lane edge still advances to the next tab
        // stop at column 4, so the word starts at column 4.
        let runs = summarize_line("\tab", &[tok(1..3, 1)], None);
        assert_eq!(runs, vec![run(4, 2, 1)]);
    }

    #[test]
    fn summarize_line_clamps_columns() {
        let line = "x".repeat(200);
        let runs = summarize_line(&line, &[], None);
        assert_eq!(
            runs,
            vec![run(2, 118, 0)],
            "content fills the lane edge to the 120-column cap"
        );
    }

    #[test]
    fn summarize_line_twelfth_run_swallows_overflow() {
        // 20 space-separated distinct-class chars would be 20 runs, but the 12th
        // run absorbs everything from its start to the last char.
        let line: String = (0..20).map(|_| "x ").collect();
        let tokens: Vec<LineToken> = (0..20)
            .map(|i| tok(i * 2..i * 2 + 1, (i % 3 + 1) as u8))
            .collect();
        let runs = summarize_line(&line, &tokens, None);
        assert_eq!(runs.len(), 12, "runs cap at twelve");
        let last = runs[11];
        // The last char sits at display column 40 (x at even columns, shifted
        // past the 2-column lane); run 12 stretches to cover it.
        assert_eq!(last.start_col as u32 + last.len as u32, 41);
    }

    /// An edit can insert rows past the build cursor, since the cursor sits
    /// wherever the last chunk stopped. It summarizes those rows itself and the
    /// cursor advances by the same delta, so the summarized prefix and the
    /// claimed one stay equal and the build resumes at the seam.
    #[test]
    fn an_edit_reaching_past_the_build_cursor_summarizes_what_it_inserts() {
        let total = BUILD_CHUNK as usize + 904;
        let before = rope(&vec!["line"; total].join("\n"));
        let mut content = MinimapContent::new(1);

        // One chunk, leaving the cursor at BUILD_CHUNK with the tail unbuilt.
        content.sync(
            &before,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        let _ = content.take_queued();
        assert_eq!(content.lines.len(), BUILD_CHUNK as usize, "one chunk built");

        // Three lines open on the cursor's last built row, so the rows they
        // occupy run past where the build had reached. Every line is "line\n", so
        // a row starts every five bytes.
        let seam = 5 * (BUILD_CHUNK as usize - 1);
        let mut after: Vec<&str> = vec!["line"; total];
        after.splice(
            BUILD_CHUNK as usize - 1..BUILD_CHUNK as usize - 1,
            ["new"; 3],
        );
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
                (BUILD_CHUNK - 1, 1, 4),
                (BUILD_CHUNK + 3, 0, total - BUILD_CHUNK as usize),
            ],
            "the edit removes the one row it replaced, and the build resumes at \
             the row after the four it left, with no gap and no overlap",
        );
        assert_eq!(
            content.lines.len(),
            total + 3,
            "the store holds every row, the edit's included",
        );

        let inserted = summarize_line("new", &[], None);
        assert_eq!(
            &content.lines[BUILD_CHUNK as usize - 1..BUILD_CHUNK as usize + 2],
            &[inserted.clone(), inserted.clone(), inserted][..],
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
        let total = BUILD_CHUNK as usize + 904;
        let before = rope(&vec!["line"; total].join("\n"));
        let mut content = MinimapContent::new(1);

        content.sync(
            &before,
            1,
            &Patch::empty(),
            versions(0, 0),
            no_tokens,
            no_edges,
        );
        let _ = content.take_queued();
        assert_eq!(content.lines.len(), BUILD_CHUNK as usize, "one chunk built");

        // Four rows collapse into one, starting two rows below the cursor, so two
        // of the four were built and two were not. Every line is "line\n", so a
        // row starts every five bytes and a row's last byte is its newline.
        let first = BUILD_CHUNK as usize - 2;
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
        let mut content = MinimapContent::new(1);
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
            summarize_line("CHARLIE", &[], None),
            "and the store holds the rewritten line at that row",
        );
    }

    #[test]
    fn single_line_edit_queues_one_one_line_splice() {
        let before = rope("alpha\nbeta\ngamma\n");
        let mut content = MinimapContent::new(1);
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
        let mut content = MinimapContent::new(1);

        // The initial build queries exactly the build chunk it fills.
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
            "the build queries only the chunk it fills (6 lines and a trailing empty line)"
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
        let mut content = MinimapContent::new(1);
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

    #[test]
    fn multi_line_replace_removes_the_old_span() {
        let before = rope("a\nb\nc\nd\ne\n");
        let mut content = MinimapContent::new(1);
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
    fn chunked_build_appends_until_complete() {
        let total = BUILD_CHUNK + BUILD_CHUNK / 2;
        // Exactly `total` lines, with no trailing newline that would add an empty
        // last line.
        let text: String = vec!["line"; total as usize].join("\n");
        let rope = rope(&text);
        let mut content = MinimapContent::new(1);

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
            BUILD_CHUNK,
            "first chunk is full"
        );
        assert!(
            content.build_pending(),
            "the build still has a chunk after the first sync"
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
        assert_eq!(second[0].start, BUILD_CHUNK);
        assert_eq!(
            second[0].lines.len() as u32,
            BUILD_CHUNK / 2,
            "the remainder finishes the build",
        );
        assert!(
            !content.build_pending(),
            "the build is complete after the last chunk"
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

    fn built_recolor_fixture() -> (Rope, MinimapContent) {
        let total = RESYNC_CHUNK + RESYNC_CHUNK / 2;
        let text: String = vec!["line"; total as usize].join("\n");
        let rope = rope(&text);
        let mut content = MinimapContent::new(1);
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

    /// A monochrome strip short enough to build and sweep in one sync, so a
    /// scoped sweep's row set is exactly what the splices report.
    ///
    /// Left settled at syntax version 1 with nothing pending, since a fresh
    /// content starts out owing a full sweep it has not yet been asked for.
    fn small_recolor_fixture(lines: u32) -> (Rope, MinimapContent) {
        let rope = rope(&vec!["line"; lines as usize].join("\n"));
        let mut content = MinimapContent::new(1);
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

    #[test]
    fn a_bump_landing_mid_sweep_falls_back_to_everything() {
        let total = RESYNC_CHUNK + RESYNC_CHUNK / 2;
        let (rope, mut content) = built_recolor_fixture();

        // Park a full sweep's cursor at RESYNC_CHUNK.
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

        // A second parse lands mid-sweep reporting one row. The rows already
        // swept carry version 1's colors, so the new sweep cannot trust that
        // row alone and must start over.
        content.note_syntax_rows(Some(9000..9001));
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
            restarted.first().map(|s| s.start),
            Some(0),
            "the restarted sweep covers the rows the first one already touched",
        );

        while content.build_pending() {
            content.sync(
                &rope,
                1,
                &Patch::empty(),
                parse_versions(0, 2),
                color(2),
                no_edges,
            );
        }
        let recolored = summarize_line("line", &[tok(0..4, 2)], None);
        let stale: Vec<usize> = content
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| **line != recolored)
            .map(|(row, _)| row)
            .collect();
        assert!(
            stale.is_empty(),
            "the fallback sweep left rows {stale:?} on the previous version's runs"
        );
        assert_eq!(content.lines.len(), total as usize);
    }

    #[test]
    fn a_recolor_sweeps_in_chunks_across_syncs() {
        let total = RESYNC_CHUNK + RESYNC_CHUNK / 2;
        let (rope, mut content) = built_recolor_fixture();

        // A syntax bump recolors, one chunk per sync.
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
            "the chunk's changed rows are contiguous, so they ship as one splice"
        );
        assert_eq!(first[0].start, 0);
        assert_eq!(
            (first[0].removed, first[0].lines.len()),
            (RESYNC_CHUNK, RESYNC_CHUNK as usize),
            "and that splice still covers the whole chunk"
        );
        assert_eq!(
            first[0].lines[0],
            summarize_line("line", &[tok(0..4, 1)], None),
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
            (total - RESYNC_CHUNK) as usize,
            "the remainder finishes the sweep"
        );
        assert_eq!(second[0].start, RESYNC_CHUNK);

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

    #[test]
    fn a_fresh_syntax_bump_restarts_the_sweep() {
        let (rope, mut content) = built_recolor_fixture();

        // The first recolor sweeps chunk 0 to class 1, leaving the cursor mid-file.
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 1),
            color(1),
            no_edges,
        );
        content.take_queued();

        // A fresh bump to a new version restarts at row 0 with the new color
        // instead of continuing to the next chunk.
        content.sync(
            &rope,
            1,
            &Patch::empty(),
            versions(0, 2),
            color(2),
            no_edges,
        );
        let restarted = content.take_queued();
        assert_eq!(
            restarted[0].start, 0,
            "a fresh bump restarts the sweep at the top"
        );
        assert_eq!(restarted.len(), 1, "the restarted chunk ships coalesced");
        assert_eq!(restarted[0].lines.len(), RESYNC_CHUNK as usize);
        assert_eq!(
            restarted[0].lines[0],
            summarize_line("line", &[tok(0..4, 2)], None)
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
            scopes + 1 + 6,
            "default foreground, one color per syntax scope, then six edge classes",
        );
    }

    #[test]
    fn summarize_line_prepends_the_edge_lane() {
        // An edge fills the reserved cols 0-1 and content starts at col 2.
        let runs = summarize_line("ab", &[tok(0..2, 1)], Some(40));
        assert_eq!(runs, vec![run(0, 2, 40), run(2, 2, 1)]);
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

    #[test]
    fn build_carries_each_line_edge() {
        let text = rope("alpha\nbravo");
        let mut content = MinimapContent::new(1);
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
            built[0][0],
            run(2, 5, 0),
            "an unmarked line starts past the lane"
        );
        assert_eq!(
            built[1][0],
            run(0, 2, 40),
            "a marked line leads with its edge"
        );
    }

    /// The frame loop only ticks more syncs while `build_pending` holds, so a
    /// sweep that reports false after its first chunk strands every row past it
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
        let total = RESYNC_CHUNK + RESYNC_CHUNK / 2;
        let (before, mut content) = built_recolor_fixture();

        // Sweep the first chunk, parking the cursor at RESYNC_CHUNK.
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

        let recolored = summarize_line("line", &[tok(0..4, 1)], None);
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
        let total = RESYNC_CHUNK + RESYNC_CHUNK / 2;
        let (before, mut content) = built_recolor_fixture();

        // Sweep the first chunk, parking the cursor at RESYNC_CHUNK.
        content.sync(
            &before,
            1,
            &Patch::empty(),
            versions(0, 1),
            color(1),
            no_edges,
        );
        content.take_queued();

        // Five lines open at the top, sliding the cursor to RESYNC_CHUNK + 5, and
        // one more opens two rows below where the cursor lands. Every line is
        // "line\n", so a row starts every five bytes.
        let after = rope(&vec!["line"; total as usize + 6].join("\n"));
        let second = 5 * (RESYNC_CHUNK as usize + 2);
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

        let recolored = summarize_line("line", &[tok(0..4, 1)], None);
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
        let mut content = MinimapContent::new(1);
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
        let mut content = MinimapContent::new(1);
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

    /// A decoration bump runs on every diff recompute and diagnostic batch, and
    /// usually moves no mark, so it must not resolve tokens for the whole built
    /// range just to discover there is nothing to do.
    #[test]
    fn an_edge_resync_that_changes_nothing_fetches_no_tokens() {
        let text = rope("alpha\nbravo\ncharlie");
        let mut content = MinimapContent::new(1);
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

    #[test]
    fn an_edge_resync_fetches_only_the_changed_rows() {
        let text = rope("alpha\nbravo\ncharlie\ndelta\necho");
        let mut content = MinimapContent::new(1);
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
        let mut content = MinimapContent::new(1);

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
        let mut content = MinimapContent::new(1);
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
        let mut content = MinimapContent::new(1);
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
        let mut content = MinimapContent::new(1);
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
            summarize_line("bravo", &[], None),
            "the line re-summarizes without its lane",
        );
    }

    #[test]
    fn decoration_change_splices_only_the_marked_line() {
        let text = rope("alpha\nbravo\ncharlie");
        let mut content = MinimapContent::new(1);

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
            splices[0].lines[0][0],
            run(0, 2, 40),
            "the mark leads the line"
        );
    }

    #[test]
    fn syntax_change_resummarizes_content() {
        let text = rope("alpha\nbeta");
        let mut content = MinimapContent::new(1);

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
            splices[0].lines[0],
            vec![run(2, 5, 0)],
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
        let mut content = MinimapContent::new(1);
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
                summarize_line("alpha", &[tok(0..4, 2)], None),
                summarize_line("bravo", &[tok(0..4, 2)], Some(40)),
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

    #[test]
    fn minimap_top_maps_the_viewport_across_the_file() {
        use super::minimap_top;

        assert_eq!(minimap_top(40.0, 120.0, 0.0, 30.0), 0.0, "a fitted file");

        let mid = minimap_top(200.0, 80.0, 85.0, 30.0);
        assert!(
            (mid - 60.0).abs() < 1e-4,
            "half-scrolled lands mid-strip: {mid}"
        );

        assert_eq!(
            minimap_top(200.0, 80.0, 1_000.0, 30.0),
            120.0,
            "a view past the end clamps to the span bottom"
        );
    }

    #[test]
    fn click_target_line_centers_within_the_cell_row() {
        use super::{click_target_line, minimap_top};

        assert_eq!(
            click_target_line(10, 3, 50.0, 0.0, 20.0),
            28,
            "cell row 3 of a fitted file centers on line 3*8+4"
        );
        assert_eq!(
            click_target_line(10, 40, 50.0, 0.0, 20.0),
            76,
            "a row past the strip clamps to the last cell"
        );

        let top = minimap_top(800.0, 80.0, 400.0, 20.0);
        assert_eq!(
            click_target_line(10, 0, 800.0, 400.0, 20.0),
            (top + 4.0) as u32,
            "a slid window shifts the target by minimap_top"
        );
    }

    #[test]
    fn oversized_buffer_disables_and_queues_nothing() {
        let text: String = (0..MAX_LINES + 1).map(|_| "x\n").collect();
        let rope = rope(&text);
        let mut content = MinimapContent::new(1);

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
