//! Pure per-line run summaries feeding the stoatty minimap strip.
//!
//! A [`MinimapContent`] holds one summary per buffer line, updated incrementally
//! from buffer edits and filled progressively for a freshly opened file, and
//! drains the changes as [`Splice`]s the emission layer turns into `minimap_lines`
//! frames. [`summarize_line`] compresses a line into colored [`Run`] blocks, never
//! the file text. Nothing here does IO, so the whole module is unit-tested.

use crate::{
    display_map::{highlights::HighlightStyleId, syntax_theme::SyntaxStyles},
    theme::Theme,
};
use ratatui::style::Color;
use std::collections::HashMap;
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

/// A single colored run on one line, `len` display columns from `start_col`
/// drawn in palette class `class`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Run {
    pub start_col: u8,
    pub len: u8,
    pub class: u8,
}

/// A token's byte range within a line, already resolved to its palette class.
///
/// The style-to-class resolution lives in [`ClassTable`]; [`summarize_line`] takes
/// the resolved class so it stays free of the theme.
#[derive(Clone, Debug)]
pub struct LineToken {
    pub range: std::ops::Range<usize>,
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

/// The run summaries of one buffer, plus the incremental-sync bookkeeping.
///
/// Mirrors the terminal's content store: one entry per line, spliced as the
/// buffer changes. Retains the rope it last synced against so an edit's old byte
/// range resolves to old rows, and a build cursor so a fresh file fills a chunk
/// at a time.
pub struct MinimapContent {
    content_id: u32,
    lines: Vec<Vec<Run>>,
    synced_version: u64,
    synced_rope: Rope,
    built_upto: u32,
    disabled: bool,
    queued: Vec<Splice>,
}

impl MinimapContent {
    pub fn new(content_id: u32) -> MinimapContent {
        MinimapContent {
            content_id,
            lines: Vec::new(),
            synced_version: 0,
            synced_rope: Rope::new(),
            built_upto: 0,
            disabled: false,
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
    /// `edits` is the buffer's `edits_since(self.synced_version())`; `line_tokens`
    /// resolves a row's tokens (byte ranges within the line plus class) for
    /// re-summarizing. Edits within the already-built prefix queue splices. The
    /// unbuilt tail fills up to [`BUILD_CHUNK`] lines per call. A buffer over
    /// [`MAX_LINES`] disables and queues nothing.
    pub fn sync(
        &mut self,
        new_rope: &Rope,
        version: u64,
        edits: &Patch<usize>,
        line_tokens: impl Fn(u32, &str) -> Vec<LineToken>,
    ) {
        if self.disabled {
            return;
        }

        let total = line_count(new_rope);
        if total as usize > MAX_LINES {
            self.disabled = true;
            self.lines.clear();
            self.queued.clear();
            self.built_upto = 0;
            self.synced_version = version;
            self.synced_rope = new_rope.clone();
            return;
        }

        if version != self.synced_version {
            for edit in edits.edits() {
                self.apply_edit(edit, new_rope, &line_tokens);
            }
            self.synced_version = version;
            self.synced_rope = new_rope.clone();
        }

        if self.built_upto < total {
            let end = (self.built_upto + BUILD_CHUNK).min(total);
            let lines = summarize_rows(new_rope, self.built_upto..end, &line_tokens);
            self.queued.push(Splice {
                start: self.built_upto,
                removed: 0,
                lines: lines.clone(),
            });
            self.lines.extend(lines);
            self.built_upto = end;
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
        line_tokens: &impl Fn(u32, &str) -> Vec<LineToken>,
    ) {
        let old_start_row = self.synced_rope.offset_to_point(edit.old.start).row;
        if old_start_row >= self.built_upto {
            return;
        }
        let old_end_row = self.synced_rope.offset_to_point(edit.old.end).row;

        let new_start_row = new_rope.offset_to_point(edit.new.start).row;
        let new_end_row = new_rope.offset_to_point(edit.new.end).row;

        let removed = (old_end_row + 1).min(self.built_upto) - old_start_row;
        let inserted = summarize_rows(new_rope, new_start_row..new_end_row + 1, line_tokens);

        let start = old_start_row as usize;
        let end = (start + removed as usize).min(self.lines.len());
        self.lines.splice(start..end, inserted.iter().cloned());

        let delta = inserted.len() as i64 - removed as i64;
        self.built_upto = (self.built_upto as i64 + delta).max(0) as u32;

        self.queued.push(Splice {
            start: old_start_row,
            removed,
            lines: inserted,
        });
    }
}

/// Summarize each row in `rows`, one [`LineToken`] set per row from `line_tokens`.
fn summarize_rows(
    rope: &Rope,
    rows: std::ops::Range<u32>,
    line_tokens: &impl Fn(u32, &str) -> Vec<LineToken>,
) -> Vec<Vec<Run>> {
    rows.map(|row| {
        let text = line_text(rope, row);
        summarize_line(&text, &line_tokens(row, &text))
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

/// Compress `line` into colored run blocks.
///
/// Walks the line by display column, a tab advancing to the next multiple of
/// [`TAB_WIDTH`] and other chars advancing one, capped at [`MAX_COLUMNS`]. A
/// non-whitespace char extends the current run when it is contiguous and shares
/// the covering token's class, otherwise opens a new run. Whitespace ends the
/// current run, so a gap breaks the blocks. A char uncovered by any token is
/// class 0. Once [`MAX_RUNS`] runs exist, the last run swallows the rest of the
/// line.
pub fn summarize_line(line: &str, tokens: &[LineToken]) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    let mut col: u32 = 0;
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
            by_style.insert(style_id, (index + 1) as u8);
            let fg = styles.interner[style_id].foreground.unwrap_or(Color::White);
            palette.push(color_to_rgb(fg));
        }

        ClassTable { palette, by_style }
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
fn color_to_rgb(color: Color) -> [u8; 3] {
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
    use super::{summarize_line, LineToken, MinimapContent, Run, Splice, BUILD_CHUNK, MAX_LINES};
    use stoat_text::{patch::Patch, Rope};

    fn rope(text: &str) -> Rope {
        Rope::from(text)
    }

    fn no_tokens(_: u32, _: &str) -> Vec<LineToken> {
        Vec::new()
    }

    fn run(start_col: u8, len: u8, class: u8) -> Run {
        Run {
            start_col,
            len,
            class,
        }
    }

    fn tok(range: std::ops::Range<usize>, class: u8) -> LineToken {
        LineToken { range, class }
    }

    #[test]
    fn summarize_line_coalesces_and_breaks_on_whitespace() {
        // "ab cd" with one token over "ab" and one over "cd": a run per word,
        // broken by the space, each carrying its token's class.
        let runs = summarize_line("ab cd", &[tok(0..2, 1), tok(3..5, 2)]);
        assert_eq!(runs, vec![run(0, 2, 1), run(3, 2, 2)]);
    }

    #[test]
    fn summarize_line_coalesces_adjacent_same_class() {
        // Two tokens of the same class over contiguous chars merge into one run.
        let runs = summarize_line("abcd", &[tok(0..2, 1), tok(2..4, 1)]);
        assert_eq!(runs, vec![run(0, 4, 1)]);
    }

    #[test]
    fn summarize_line_uncovered_is_class_zero() {
        let runs = summarize_line("ab", &[]);
        assert_eq!(runs, vec![run(0, 2, 0)]);
    }

    #[test]
    fn summarize_line_expands_tabs_to_stops() {
        // A leading tab advances to column 4, so the word starts at column 4.
        let runs = summarize_line("\tab", &[tok(1..3, 1)]);
        assert_eq!(runs, vec![run(4, 2, 1)]);
    }

    #[test]
    fn summarize_line_clamps_columns() {
        let line = "x".repeat(200);
        let runs = summarize_line(&line, &[]);
        assert_eq!(runs, vec![run(0, 120, 0)], "columns cap at 120");
    }

    #[test]
    fn summarize_line_twelfth_run_swallows_overflow() {
        // 20 space-separated distinct-class chars would be 20 runs, but the 12th
        // run absorbs everything from its start to the last char.
        let line: String = (0..20).map(|_| "x ").collect();
        let tokens: Vec<LineToken> = (0..20)
            .map(|i| tok(i * 2..i * 2 + 1, (i % 3 + 1) as u8))
            .collect();
        let runs = summarize_line(&line, &tokens);
        assert_eq!(runs.len(), 12, "runs cap at twelve");
        let last = runs[11];
        // The last char sits at display column 38 (x at even columns); run 12
        // stretches to cover it.
        assert_eq!(last.start_col as u32 + last.len as u32, 39);
    }

    #[test]
    fn single_line_edit_queues_one_one_line_splice() {
        let before = rope("alpha\nbeta\ngamma\n");
        let mut content = MinimapContent::new(1);
        content.sync(&before, 1, &Patch::empty(), no_tokens);
        content.take_queued();

        // Replace "beta" (line 1) in place.
        let after = rope("alpha\nBETA!\ngamma\n");
        let edit = Patch::new(vec![stoat_text::patch::Edit {
            old: 6..10,
            new: 6..11,
        }]);
        content.sync(&after, 2, &edit, no_tokens);

        let queued = content.take_queued();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].start, 1);
        assert_eq!(queued[0].removed, 1);
        assert_eq!(queued[0].lines.len(), 1, "one line re-summarized");
    }

    #[test]
    fn newline_insertion_inserts_one_more_than_removed() {
        let before = rope("alpha\nbeta\ngamma\n");
        let mut content = MinimapContent::new(1);
        content.sync(&before, 1, &Patch::empty(), no_tokens);
        content.take_queued();

        // Insert a newline inside "beta", splitting line 1 into two.
        let after = rope("alpha\nbe\nta\ngamma\n");
        let edit = Patch::new(vec![stoat_text::patch::Edit {
            old: 8..8,
            new: 8..9,
        }]);
        content.sync(&after, 2, &edit, no_tokens);

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
        content.sync(&before, 1, &Patch::empty(), no_tokens);
        content.take_queued();

        // Replace lines 1..=3 ("b\nc\nd") with one line "X".
        let after = rope("a\nX\ne\n");
        let edit = Patch::new(vec![stoat_text::patch::Edit {
            old: 2..7,
            new: 2..3,
        }]);
        content.sync(&after, 2, &edit, no_tokens);

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

        content.sync(&rope, 1, &Patch::empty(), no_tokens);
        let first = content.take_queued();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].start, 0);
        assert_eq!(
            first[0].lines.len() as u32,
            BUILD_CHUNK,
            "first chunk is full"
        );

        content.sync(&rope, 1, &Patch::empty(), no_tokens);
        let second = content.take_queued();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].start, BUILD_CHUNK);
        assert_eq!(
            second[0].lines.len() as u32,
            BUILD_CHUNK / 2,
            "the remainder finishes the build",
        );

        content.sync(&rope, 1, &Patch::empty(), no_tokens);
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
            scopes + 1,
            "the default foreground plus one color per syntax scope",
        );
    }

    #[test]
    fn oversized_buffer_disables_and_queues_nothing() {
        let text: String = (0..MAX_LINES + 1).map(|_| "x\n").collect();
        let rope = rope(&text);
        let mut content = MinimapContent::new(1);

        content.sync(&rope, 1, &Patch::empty(), no_tokens);

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
