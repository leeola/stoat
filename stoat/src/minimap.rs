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
    Point, Rope,
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
    /// Edge-lane class per built line, kept parallel to [`Self::lines`] so a diff
    /// or diagnostic change re-summarizes only the lines whose mark changed.
    edges: Vec<Option<u8>>,
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
            queued: Vec::new(),
        }
    }

    pub fn content_id(&self) -> u32 {
        self.content_id
    }

    /// The buffer version this content last synced against, the argument the
    /// caller passes to `edits_since` to collect the changes for the next sync.
    pub fn synced_version(&self) -> u64 {
        self.synced_version
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
    /// `edge_of` a row's diff/diagnostic mark. [`SyncVersions::decoration`]
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
        edge_of: impl Fn(u32) -> Option<u8>,
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
                self.apply_edit(edit, new_rope, &tokens_for, &edge_of);
            }
            self.synced_version = version;
            self.synced_rope = new_rope.clone();
        }

        if self.built_upto < total {
            let end = (self.built_upto + BUILD_CHUNK).min(total);
            let tokens = tokens_for(self.built_upto..end);
            let line_tokens = |row: u32, _: &str| tokens.get(&row).cloned().unwrap_or_default();
            let lines = summarize_rows(new_rope, self.built_upto..end, &line_tokens, &edge_of);
            self.queued.push(Splice {
                start: self.built_upto,
                removed: 0,
                lines: lines.clone(),
            });
            self.edges.extend((self.built_upto..end).map(&edge_of));
            self.lines.extend(lines);
            self.built_upto = end;
        }

        // A recolor rewrites the content runs, so re-summarize every built line
        // and compare. This subsumes the edge re-check, so run it instead of
        // resync_edges and advance both versions.
        if versions.syntax != self.synced_syntax_version {
            self.resync_all(new_rope, &tokens_for, &edge_of);
            self.synced_syntax_version = versions.syntax;
            self.synced_decoration_version = versions.decoration;
        } else if versions.decoration != self.synced_decoration_version {
            self.resync_edges(new_rope, &tokens_for, &edge_of);
            self.synced_decoration_version = versions.decoration;
        }
    }

    /// Re-summarize every built line and queue a one-line splice where its full
    /// summary changed, for a recolor (highlight toggle or a completed parse)
    /// that leaves the buffer text untouched.
    ///
    /// Costlier than [`Self::resync_edges`] since every line re-summarizes, but a
    /// recolor is rare and can touch any line, unlike an edge change.
    fn resync_all(
        &mut self,
        new_rope: &Rope,
        tokens_for: &impl Fn(Range<u32>) -> HashMap<u32, Vec<LineToken>>,
        edge_of: &impl Fn(u32) -> Option<u8>,
    ) {
        let tokens = tokens_for(0..self.built_upto);
        let line_tokens = |row: u32, _: &str| tokens.get(&row).cloned().unwrap_or_default();
        for row in 0..self.built_upto {
            let edge = edge_of(row);
            let text = line_text(new_rope, row);
            let summary = summarize_line(&text, &line_tokens(row, &text), edge);
            if self.lines[row as usize] == summary {
                continue;
            }
            self.lines[row as usize] = summary.clone();
            self.edges[row as usize] = edge;
            self.queued.push(Splice {
                start: row,
                removed: 1,
                lines: vec![summary],
            });
        }
    }

    /// Re-check every built line's edge mark, re-summarizing and queueing a
    /// one-line splice only where the mark changed.
    ///
    /// The syntax content is unaffected by a diff or diagnostic change, so only
    /// the changed lines pay for a re-summarize.
    fn resync_edges(
        &mut self,
        new_rope: &Rope,
        tokens_for: &impl Fn(Range<u32>) -> HashMap<u32, Vec<LineToken>>,
        edge_of: &impl Fn(u32) -> Option<u8>,
    ) {
        let tokens = tokens_for(0..self.built_upto);
        let line_tokens = |row: u32, _: &str| tokens.get(&row).cloned().unwrap_or_default();
        for row in 0..self.built_upto {
            let new_edge = edge_of(row);
            if self.edges[row as usize] == new_edge {
                continue;
            }
            let text = line_text(new_rope, row);
            let summary = summarize_line(&text, &line_tokens(row, &text), new_edge);
            self.lines[row as usize] = summary.clone();
            self.edges[row as usize] = new_edge;
            self.queued.push(Splice {
                start: row,
                removed: 1,
                lines: vec![summary],
            });
        }
    }

    /// Apply one edit that falls inside the built prefix, re-summarizing the new
    /// rows and shifting the build cursor by the line delta.
    ///
    /// An edit starting past the build cursor is left for the chunked build to
    /// summarize fresh when it reaches those rows.
    fn apply_edit(
        &mut self,
        edit: &Edit<usize>,
        new_rope: &Rope,
        tokens_for: &impl Fn(Range<u32>) -> HashMap<u32, Vec<LineToken>>,
        edge_of: &impl Fn(u32) -> Option<u8>,
    ) {
        let old_start_row = self.synced_rope.offset_to_point(edit.old.start).row;
        if old_start_row >= self.built_upto {
            return;
        }
        let old_end_row = self.synced_rope.offset_to_point(edit.old.end).row;

        let new_start_row = new_rope.offset_to_point(edit.new.start).row;
        let new_end_row = new_rope.offset_to_point(edit.new.end).row;

        let tokens = tokens_for(new_start_row..new_end_row + 1);
        let line_tokens = |row: u32, _: &str| tokens.get(&row).cloned().unwrap_or_default();

        let removed = (old_end_row + 1).min(self.built_upto) - old_start_row;
        let inserted = summarize_rows(
            new_rope,
            new_start_row..new_end_row + 1,
            &line_tokens,
            edge_of,
        );
        let inserted_edges: Vec<Option<u8>> =
            (new_start_row..new_end_row + 1).map(edge_of).collect();

        let start = old_start_row as usize;
        let end = (start + removed as usize).min(self.lines.len());
        self.lines.splice(start..end, inserted.iter().cloned());
        let edge_end = end.min(self.edges.len());
        self.edges.splice(start..edge_end, inserted_edges);

        let delta = inserted.len() as i64 - removed as i64;
        self.built_upto = (self.built_upto as i64 + delta).max(0) as u32;

        self.queued.push(Splice {
            start: old_start_row,
            removed,
            lines: inserted,
        });
    }
}

/// Summarize each row in `rows`, one [`LineToken`] set per row from `line_tokens`
/// and its edge mark from `edge_of`.
fn summarize_rows(
    rope: &Rope,
    rows: Range<u32>,
    line_tokens: &impl Fn(u32, &str) -> Vec<LineToken>,
    edge_of: &impl Fn(u32) -> Option<u8>,
) -> Vec<Vec<Run>> {
    rows.map(|row| {
        let text = line_text(rope, row);
        summarize_line(&text, &line_tokens(row, &text), edge_of(row))
    })
    .collect()
}

/// The text of `row`, without its line terminator.
fn line_text(rope: &Rope, row: u32) -> String {
    let start = rope.point_to_offset(Point::new(row, 0));
    let end = rope.point_to_offset(Point::new(row, rope.line_len(row)));
    rope.slice(start..end).to_string()
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
    by_color: HashMap<[u8; 3], u8>,
    edge_base: u8,
}

impl ClassTable {
    pub fn from_theme(theme: &Theme) -> ClassTable {
        let styles = SyntaxStyles::from_theme(theme);
        let default_fg = color_to_rgb(theme.get("ui.text").fg.unwrap_or(Color::White));

        let mut palette = vec![default_fg];
        let mut by_style = HashMap::new();
        let mut by_color = HashMap::new();
        for index in 0..styles.theme_keys().len() {
            let Some(style_id) = styles.id_for_highlight(HighlightId(index as u32)) else {
                palette.push(default_fg);
                continue;
            };
            let class = (index + 1) as u8;
            by_style.insert(style_id, class);
            let fg = styles.interner[style_id].foreground.unwrap_or(Color::White);
            let rgb = color_to_rgb(fg);
            by_color.entry(rgb).or_insert(class);
            palette.push(rgb);
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
            by_color,
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

    /// The class a token whose resolved foreground is `color` maps to, or 0 when
    /// no syntax scope draws in that color.
    ///
    /// The emission layer resolves highlights to [`Color`] rather than a
    /// [`HighlightStyleId`], so this bridges a rendered token's foreground to its
    /// palette class where [`Self::class_of`] would need the interned id.
    pub fn class_of_color(&self, color: Color) -> u8 {
        self.by_color
            .get(&color_to_rgb(color))
            .copied()
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
        summarize_line, LineToken, MinimapContent, Run, Splice, SyncVersions, BUILD_CHUNK,
        MAX_LINES,
    };
    use std::{collections::HashMap, ops::Range};
    use stoat_text::{patch::Patch, Rope};

    fn rope(text: &str) -> Rope {
        Rope::from(text)
    }

    fn no_tokens(_: Range<u32>) -> HashMap<u32, Vec<LineToken>> {
        HashMap::new()
    }

    fn no_edges(_: u32) -> Option<u8> {
        None
    }

    fn versions(decoration: u64, syntax: u64) -> SyncVersions {
        SyncVersions { decoration, syntax }
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
        let edit = Patch::new(vec![stoat_text::patch::Edit {
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
        let edit = Patch::new(vec![stoat_text::patch::Edit {
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
        let edit = Patch::new(vec![stoat_text::patch::Edit {
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
        let edit = Patch::new(vec![stoat_text::patch::Edit {
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

    #[test]
    fn class_of_color_bridges_foreground_to_class() {
        use super::ClassTable;
        use crate::theme::Theme;
        use ratatui::style::Color;

        let table = ClassTable::from_theme(&Theme::empty());
        let palette = table.palette();

        let [r, g, b] = palette[1];
        let class = table.class_of_color(Color::Rgb(r, g, b));
        assert!(class >= 1, "a scope foreground maps to a syntax class");
        assert_eq!(
            palette[class as usize], palette[1],
            "the mapped class paints the queried color",
        );

        if !palette.contains(&[1, 2, 3]) {
            assert_eq!(
                table.class_of_color(Color::Rgb(1, 2, 3)),
                0,
                "a foreground no scope uses is the default class",
            );
        }
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
