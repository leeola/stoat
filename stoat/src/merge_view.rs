use crate::{
    review,
    review::{ReviewRow, ReviewSide},
};
use std::{ops::Range, sync::Arc};
use stoat_language::Language;

/// One row of the three-column merge view, aligning the common ancestor with
/// the two sides of a conflict.
///
/// A row either pairs a single ancestor line with the ours and theirs lines
/// that descend from it (`base` set), or carries a one-sided insertion with no
/// ancestor line (`base` None). `conflict` is set when both sides changed the
/// ancestor line the row covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MergeRow {
    pub base: Option<ReviewSide>,
    pub ours: Option<ReviewSide>,
    pub theirs: Option<ReviewSide>,
    pub conflict: bool,
}

/// Align `ancestor` against `ours` and `theirs` into three-column merge rows.
///
/// Each side is diffed against the ancestor, then the two aligned walks are
/// merged on their shared ancestor lines. Rows for the same ancestor line pair
/// into one [`MergeRow`], and side-only insertions interleave after their
/// preceding ancestor line with ours before theirs. A row is a conflict when
/// both sides changed the ancestor line it covers.
pub(crate) fn build_merge_rows(
    ancestor: &str,
    ours: &str,
    theirs: &str,
    language: Option<&Arc<Language>>,
) -> Vec<MergeRow> {
    let ours_rows = review::aligned_rows(ancestor, ours, language);
    let theirs_rows = review::aligned_rows(ancestor, theirs, language);

    let mut rows = Vec::new();
    let mut i = 0;
    let mut j = 0;

    loop {
        while let Some((None, right, _)) = ours_rows.get(i).map(row_parts) {
            rows.push(MergeRow {
                base: None,
                ours: right.cloned(),
                theirs: None,
                conflict: false,
            });
            i += 1;
        }
        while let Some((None, right, _)) = theirs_rows.get(j).map(row_parts) {
            rows.push(MergeRow {
                base: None,
                ours: None,
                theirs: right.cloned(),
                conflict: false,
            });
            j += 1;
        }

        let (Some(ours_row), Some(theirs_row)) = (ours_rows.get(i), theirs_rows.get(j)) else {
            break;
        };
        let (base, ours_side, ours_changed) = row_parts(ours_row);
        let (_, theirs_side, theirs_changed) = row_parts(theirs_row);
        rows.push(MergeRow {
            base: base.cloned(),
            ours: ours_side.cloned(),
            theirs: theirs_side.cloned(),
            conflict: ours_changed && theirs_changed,
        });
        i += 1;
        j += 1;
    }

    rows
}

/// Split a review row into its (base side, other side, is-changed) parts. `left`
/// is the base (ancestor) side and `right` the other side; a `Context` row is
/// unchanged on both, a `Changed` row is changed with either side possibly
/// absent (a deletion or an insertion).
fn row_parts(row: &ReviewRow) -> (Option<&ReviewSide>, Option<&ReviewSide>, bool) {
    match row {
        ReviewRow::Context { left, right } => (Some(left), Some(right), false),
        ReviewRow::Changed { left, right } => (left.as_ref(), right.as_ref(), true),
    }
}

/// The three-column merge of a conflicted file.
///
/// Holds the aligned merge rows and the conflict chunks carved out of them.
/// Seeds the center column of the `:conflict` view via
/// [`Self::initial_center_text`]. Each chunk's resolution state is then derived
/// from the center text with [`ConflictChunk::classify`] rather than stored, so
/// buffer edits and undo can never desync it.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct MergeDoc {
    pub(crate) rows: Vec<MergeRow>,
    pub(crate) chunks: Vec<ConflictChunk>,
}

#[allow(dead_code)]
impl MergeDoc {
    /// Align the three stages into merge rows and carve the conflict chunks out
    /// of them.
    pub(crate) fn build(
        ancestor: &str,
        ours: &str,
        theirs: &str,
        language: Option<&Arc<Language>>,
    ) -> Self {
        let rows = build_merge_rows(ancestor, ours, theirs, language);

        let mut chunks = Vec::new();
        let mut start = None;
        for (i, row) in rows.iter().enumerate() {
            match (row.conflict, start) {
                (true, None) => start = Some(i),
                (false, Some(s)) => {
                    chunks.push(ConflictChunk {
                        row_range: s..i,
                        auto: None,
                    });
                    start = None;
                },
                _ => {},
            }
        }
        if let Some(s) = start {
            chunks.push(ConflictChunk {
                row_range: s..rows.len(),
                auto: None,
            });
        }

        for chunk in &mut chunks {
            let range = chunk.row_range.clone();
            chunk.auto = indent_auto_resolution(&rows, &range);
        }

        Self { rows, chunks }
    }

    /// The initial center-column text plus the byte range each chunk occupies
    /// in it.
    ///
    /// Non-conflict rows contribute their auto-merged line, and each conflict
    /// chunk contributes one git-style marker block. The returned ranges pin
    /// each chunk's region so a later pick can reassemble just that span.
    pub(crate) fn initial_center_text(&self) -> (String, Vec<Range<usize>>) {
        let mut text = String::new();
        let mut ranges = Vec::new();

        let mut chunk_idx = 0;
        let mut i = 0;
        while i < self.rows.len() {
            if chunk_idx < self.chunks.len() && self.chunks[chunk_idx].row_range.start == i {
                let chunk = &self.chunks[chunk_idx];
                let start = text.len();
                let end_row = match &chunk.auto {
                    Some(auto) => {
                        text.push_str(&render_lines(&auto.lines));
                        auto.covered.end
                    },
                    None => {
                        text.push_str(&chunk.marker_text(&self.rows));
                        chunk.row_range.end
                    },
                };
                ranges.push(start..text.len());
                i = end_row;
                chunk_idx += 1;
                continue;
            }
            if let Some(line) = auto_merge_line(&self.rows[i]) {
                text.push_str(line);
                text.push('\n');
            }
            i += 1;
        }

        (text, ranges)
    }

    /// One entry per line of [`Self::initial_center_text`], pairing that center
    /// line with the ours and theirs side content the three-column view aligns
    /// beside it.
    ///
    /// A non-conflict row that contributes a center line emits that row's sides.
    /// A conflict chunk emits one entry per display row of its band, its ours and
    /// theirs lines top-aligned against the center.
    ///
    /// `chunk_center_rows[i]` is chunk `i`'s current center-row span, resolved
    /// from its anchors each frame. The band is sized to the tallest of that
    /// span and the two side heights, so a pick that shrank the center below a
    /// taller side still emits a row per side line. The rows past the center
    /// span line up with the padding blocks the view installs to make room. A
    /// missing entry falls back to the marker or auto-resolution height.
    pub(crate) fn align(&self, chunk_center_rows: &[usize]) -> Vec<AlignRow<'_>> {
        let mut plan = Vec::new();
        let mut chunk_idx = 0;
        let mut i = 0;
        while i < self.rows.len() {
            if chunk_idx < self.chunks.len() && self.chunks[chunk_idx].row_range.start == i {
                let chunk = &self.chunks[chunk_idx];
                let ours: Vec<&ReviewSide> = self.rows[chunk.row_range.clone()]
                    .iter()
                    .filter_map(|r| r.ours.as_ref())
                    .collect();
                let theirs: Vec<&ReviewSide> = self.rows[chunk.row_range.clone()]
                    .iter()
                    .filter_map(|r| r.theirs.as_ref())
                    .collect();
                let center_lines = match &chunk.auto {
                    Some(auto) => auto.lines.len(),
                    None => ours.len() + theirs.len() + 3,
                };
                let span = chunk_center_rows
                    .get(chunk_idx)
                    .copied()
                    .unwrap_or(center_lines);
                let display_span = span.max(ours.len()).max(theirs.len());
                for b in 0..display_span {
                    plan.push(AlignRow {
                        ours: ours.get(b).copied(),
                        theirs: theirs.get(b).copied(),
                        chunk: Some(chunk_idx),
                    });
                }
                i = chunk
                    .auto
                    .as_ref()
                    .map_or(chunk.row_range.end, |a| a.covered.end);
                chunk_idx += 1;
                continue;
            }
            let row = &self.rows[i];
            if auto_merge_line(row).is_some() {
                plan.push(AlignRow {
                    ours: row.ours.as_ref(),
                    theirs: row.theirs.as_ref(),
                    chunk: None,
                });
            }
            i += 1;
        }
        plan
    }
}

/// One center line of a [`MergeDoc`] paired with the ours and theirs side
/// content the three-column conflict view aligns beside it.
pub(crate) struct AlignRow<'a> {
    pub(crate) ours: Option<&'a ReviewSide>,
    pub(crate) theirs: Option<&'a ReviewSide>,
    /// Index into [`MergeDoc::chunks`] when this line sits in a conflict band.
    pub(crate) chunk: Option<usize>,
}

/// A maximal run of adjacent conflict rows within a [`MergeDoc`]. A non-conflict
/// row between conflicts separates one chunk from the next.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConflictChunk {
    pub(crate) row_range: Range<usize>,
    /// Set when the chunk is an indentation-only conflict that resolves
    /// automatically (one side re-indented, the other edited content).
    pub(crate) auto: Option<AutoResolution>,
}

/// An automatic resolution of an indentation-only conflict chunk.
///
/// The resolved center lines, plus the rows they replace. `covered` starts at
/// the chunk's first row and may extend past it to absorb trailing content-side
/// insertions re-indented by the same delta.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutoResolution {
    pub(crate) lines: Vec<String>,
    pub(crate) covered: Range<usize>,
}

#[allow(dead_code)]
impl ConflictChunk {
    /// The ancestor lines this chunk covers, skipping rows with no ancestor.
    pub(crate) fn base_lines<'a>(&self, rows: &'a [MergeRow]) -> Vec<&'a str> {
        rows[self.row_range.clone()]
            .iter()
            .filter_map(|r| r.base.as_ref())
            .map(|s| s.text.as_str())
            .collect()
    }

    /// The ours-side lines this chunk covers, skipping rows ours deleted.
    pub(crate) fn ours_lines<'a>(&self, rows: &'a [MergeRow]) -> Vec<&'a str> {
        rows[self.row_range.clone()]
            .iter()
            .filter_map(|r| r.ours.as_ref())
            .map(|s| s.text.as_str())
            .collect()
    }

    /// The theirs-side lines this chunk covers, skipping rows theirs deleted.
    pub(crate) fn theirs_lines<'a>(&self, rows: &'a [MergeRow]) -> Vec<&'a str> {
        rows[self.row_range.clone()]
            .iter()
            .filter_map(|r| r.theirs.as_ref())
            .map(|s| s.text.as_str())
            .collect()
    }

    /// Render this chunk as a git-style marker block. The ours lines go between
    /// `<<<<<<< ours` and `=======`, then the theirs lines up to
    /// `>>>>>>> theirs`. A side that deleted its lines yields an empty section.
    /// There is no ancestor section, since the flanking columns show it.
    pub(crate) fn marker_text(&self, rows: &[MergeRow]) -> String {
        let mut out = String::from("<<<<<<< ours\n");
        for line in self.ours_lines(rows) {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("=======\n");
        for line in self.theirs_lines(rows) {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(">>>>>>> theirs\n");
        out
    }

    /// Assemble the resolved lines for a per-row [`RowPick`], in merge-row order
    /// with the ours line before the theirs line on a row that takes both.
    /// `picks` has one entry per row in [`Self::row_range`].
    pub(crate) fn assembly_text(&self, rows: &[MergeRow], picks: &[RowPick]) -> String {
        let mut out = String::new();
        for (row, pick) in rows[self.row_range.clone()].iter().zip(picks) {
            if pick.ours
                && let Some(side) = &row.ours
            {
                out.push_str(&side.text);
                out.push('\n');
            }
            if pick.theirs
                && let Some(side) = &row.theirs
            {
                out.push_str(&side.text);
                out.push('\n');
            }
        }
        out
    }

    /// Render a partially-picked chunk as text.
    ///
    /// Each decided row emits its picked lines in merge-row order, ours before
    /// theirs. When any row is still undecided, one marker block follows over
    /// only those rows' present lines.
    ///
    /// A row is decided when its [`RowPick`] takes at least one side. An
    /// undecided row (`{false, false}`) stays in the shrinking marker block.
    ///
    /// With every row decided this equals [`Self::assembly_text`]. With every
    /// row undecided it equals [`Self::marker_text`], so classify still reads
    /// an all-undecided region as [`ChunkState::Unresolved`].
    pub(crate) fn partial_text(&self, rows: &[MergeRow], picks: &[RowPick]) -> String {
        let mut out = String::new();
        for line in self.resolution_plan(rows, picks) {
            match line {
                PlanLine::Content { row, side } => {
                    if let Some(text) = side_line(&rows[row], side) {
                        out.push_str(text);
                        out.push('\n');
                    }
                },
                PlanLine::Delimiter(text) => {
                    out.push_str(text);
                    out.push('\n');
                },
            }
        }
        out
    }

    /// Map a line of the [`Self::partial_text`] rendering back to the merge row
    /// and side it came from, or `None` for a marker delimiter line or a
    /// `row_in_region` past the rendering's last line.
    ///
    /// `row_in_region` is the zero-based line offset of the cursor within the
    /// chunk's center region. `picks` must be the picks the region currently
    /// renders, so the caller derives them from the classified state rather
    /// than a possibly-stale stored vector.
    pub(crate) fn center_row_to_merge_row(
        &self,
        rows: &[MergeRow],
        picks: &[RowPick],
        row_in_region: usize,
    ) -> Option<(usize, Side)> {
        match self.resolution_plan(rows, picks).get(row_in_region)? {
            PlanLine::Content { row, side } => Some((*row, *side)),
            PlanLine::Delimiter(_) => None,
        }
    }

    /// The ordered lines [`Self::partial_text`] emits, as their source row and
    /// side or a marker delimiter. Only present sides produce a line, so the
    /// plan's length matches the rendered line count exactly, which keeps
    /// [`Self::center_row_to_merge_row`] aligned with the text.
    fn resolution_plan(&self, rows: &[MergeRow], picks: &[RowPick]) -> Vec<PlanLine> {
        let range = self.row_range.clone();
        let mut plan = Vec::new();

        for (offset, pick) in picks.iter().enumerate() {
            let row = range.start + offset;
            if pick.ours && rows[row].ours.is_some() {
                plan.push(PlanLine::Content {
                    row,
                    side: Side::Ours,
                });
            }
            if pick.theirs && rows[row].theirs.is_some() {
                plan.push(PlanLine::Content {
                    row,
                    side: Side::Theirs,
                });
            }
        }

        let undecided: Vec<usize> = picks
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.ours && !p.theirs)
            .map(|(offset, _)| range.start + offset)
            .collect();
        if !undecided.is_empty() {
            plan.push(PlanLine::Delimiter("<<<<<<< ours"));
            for &row in &undecided {
                if rows[row].ours.is_some() {
                    plan.push(PlanLine::Content {
                        row,
                        side: Side::Ours,
                    });
                }
            }
            plan.push(PlanLine::Delimiter("======="));
            for &row in &undecided {
                if rows[row].theirs.is_some() {
                    plan.push(PlanLine::Content {
                        row,
                        side: Side::Theirs,
                    });
                }
            }
            plan.push(PlanLine::Delimiter(">>>>>>> theirs"));
        }

        plan
    }

    /// A whole-side pick taking every ours line and no theirs line.
    pub(crate) fn all_ours(&self) -> Vec<RowPick> {
        vec![
            RowPick {
                ours: true,
                theirs: false
            };
            self.row_count()
        ]
    }

    /// A whole-side pick taking every theirs line and no ours line.
    pub(crate) fn all_theirs(&self) -> Vec<RowPick> {
        vec![
            RowPick {
                ours: false,
                theirs: true
            };
            self.row_count()
        ]
    }

    /// A whole-side pick taking both sides on every row.
    pub(crate) fn all_both(&self) -> Vec<RowPick> {
        vec![
            RowPick {
                ours: true,
                theirs: true
            };
            self.row_count()
        ]
    }

    /// Derive this chunk's [`ChunkState`] by matching `region_text` (trailing
    /// newlines ignored) against the candidate renderings. An indent
    /// auto-resolution matches first, then the marker block, the
    /// ours/theirs/both whole-side assemblies, and the assembly for `picks`.
    /// Anything else is [`ChunkState::Manual`].
    pub(crate) fn classify(
        &self,
        rows: &[MergeRow],
        picks: &[RowPick],
        region_text: &str,
    ) -> ChunkState {
        let target = region_text.trim_end_matches('\n');
        let matches = |candidate: &str| candidate.trim_end_matches('\n') == target;

        if let Some(auto) = &self.auto
            && matches(&render_lines(&auto.lines))
        {
            ChunkState::AutoIndent
        } else if matches(&self.marker_text(rows)) {
            ChunkState::Unresolved
        } else if matches(&self.assembly_text(rows, &self.all_ours())) {
            ChunkState::Ours
        } else if matches(&self.assembly_text(rows, &self.all_theirs())) {
            ChunkState::Theirs
        } else if matches(&self.assembly_text(rows, &self.all_both())) {
            ChunkState::Both
        } else if matches(&self.partial_text(rows, picks)) {
            ChunkState::Picked
        } else {
            ChunkState::Manual
        }
    }

    fn row_count(&self) -> usize {
        self.row_range.end - self.row_range.start
    }
}

/// Which sides a single conflict row contributes to the assembled resolution.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RowPick {
    pub(crate) ours: bool,
    pub(crate) theirs: bool,
}

/// One side of a conflict row, naming which candidate a rendered line came from.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Ours,
    Theirs,
}

/// One line of a chunk's [`ConflictChunk::partial_text`] rendering, as the merge
/// row and side it came from or a marker delimiter that maps to no row.
enum PlanLine {
    Content { row: usize, side: Side },
    Delimiter(&'static str),
}

/// The resolution state of a conflict chunk's center region, derived from its
/// text against the candidate renderings.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChunkState {
    /// Still the raw marker block.
    Unresolved,
    /// The whole ours side.
    Ours,
    /// The whole theirs side.
    Theirs,
    /// Both sides, ours before theirs on each row.
    Both,
    /// A per-row mix that is not a whole-side pick.
    Picked,
    /// An indentation-only conflict auto-resolved at the re-indented level.
    AutoIndent,
    /// Hand-edited to text no pick produces.
    Manual,
}

/// The line a non-conflict [`MergeRow`] contributes to the auto-merged center
/// text, or `None` for a clean deletion that contributes nothing.
///
/// An insertion emits its one present side. A row with an ancestor emits
/// whichever side edited it, or the shared line when neither did.
#[allow(dead_code)]
fn auto_merge_line(row: &MergeRow) -> Option<&str> {
    match (row.base.as_ref(), row.ours.as_ref(), row.theirs.as_ref()) {
        (None, Some(side), _) | (None, None, Some(side)) => Some(side.text.as_str()),
        (Some(_), None, _) | (Some(_), _, None) => None,
        (Some(base), Some(ours), Some(theirs)) => {
            let edited = if ours.text == base.text { theirs } else { ours };
            Some(edited.text.as_str())
        },
        (None, None, None) => None,
    }
}

/// The text of `row`'s `side`, or `None` when that side deleted its line.
fn side_line(row: &MergeRow, side: Side) -> Option<&str> {
    match side {
        Side::Ours => row.ours.as_ref(),
        Side::Theirs => row.theirs.as_ref(),
    }
    .map(|s| s.text.as_str())
}

/// Render resolved center lines as newline-terminated text.
#[allow(dead_code)]
fn render_lines(lines: &[String]) -> String {
    let mut out = String::new();
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The side of a conflict a chunk designates as the one that only re-indented.
#[allow(dead_code)]
#[derive(Clone, Copy)]
enum IndentSide {
    Ours,
    Theirs,
}

/// Split a row into (indent side, content side) for the designated indent side.
#[allow(dead_code)]
fn split_sides(row: &MergeRow, side: IndentSide) -> (Option<&ReviewSide>, Option<&ReviewSide>) {
    match side {
        IndentSide::Ours => (row.ours.as_ref(), row.theirs.as_ref()),
        IndentSide::Theirs => (row.theirs.as_ref(), row.ours.as_ref()),
    }
}

/// The maximal leading run of spaces and tabs in `line`.
#[allow(dead_code)]
fn leading_ws(line: &str) -> &str {
    let end = line.find(|c| c != ' ' && c != '\t').unwrap_or(line.len());
    &line[..end]
}

/// `line` with its leading whitespace removed.
#[allow(dead_code)]
fn content_after_ws(line: &str) -> &str {
    &line[leading_ws(line).len()..]
}

/// Whether the two prefixes together use both spaces and tabs, which makes a
/// literal indent swap ambiguous without tabstop math.
#[allow(dead_code)]
fn mixes_tab_and_space(a: &str, b: &str) -> bool {
    let has_space = a.contains(' ') || b.contains(' ');
    let has_tab = a.contains('\t') || b.contains('\t');
    has_space && has_tab
}

/// The side that only re-indented base on every both-present conflict row of
/// `chunk`. `None` when neither side qualifies or no row has both sides
/// present.
#[allow(dead_code)]
fn pick_indent_side(rows: &[MergeRow], chunk: &Range<usize>) -> Option<IndentSide> {
    let both_present = rows[chunk.clone()]
        .iter()
        .any(|row| row.ours.is_some() && row.theirs.is_some());
    if !both_present {
        return None;
    }

    let qualifies = |side: IndentSide| {
        rows[chunk.clone()].iter().all(|row| {
            let Some(base) = row.base.as_ref() else {
                return true;
            };
            match split_sides(row, side) {
                (Some(indent), Some(_)) => {
                    content_after_ws(&indent.text) == content_after_ws(&base.text)
                },
                _ => true,
            }
        })
    };

    if qualifies(IndentSide::Ours) {
        Some(IndentSide::Ours)
    } else if qualifies(IndentSide::Theirs) {
        Some(IndentSide::Theirs)
    } else {
        None
    }
}

/// Attempt to auto-resolve `chunk` as an indentation-only conflict.
///
/// One side must have only re-indented base while the other edited content. The
/// content edits are re-emitted at the new indent. Returns `None` (leaving a
/// normal conflict) when no side qualifies, a content line does not sit at
/// base's indent, a space/tab conversion is involved, or an absorbed insertion
/// would need a non-uniform transform.
#[allow(dead_code)]
fn indent_auto_resolution(rows: &[MergeRow], chunk: &Range<usize>) -> Option<AutoResolution> {
    let side = pick_indent_side(rows, chunk)?;

    let mut lines = Vec::new();
    let mut uniform: Option<(String, String)> = None;
    let mut uniform_ok = true;

    for row in &rows[chunk.clone()] {
        let base = row.base.as_ref()?;
        match split_sides(row, side) {
            (Some(indent), Some(content)) => {
                let base_prefix = leading_ws(&base.text);
                let side_prefix = leading_ws(&indent.text);
                if mixes_tab_and_space(base_prefix, side_prefix) {
                    return None;
                }
                let rest = content.text.strip_prefix(base_prefix)?;
                lines.push(format!("{side_prefix}{rest}"));

                let transform = (base_prefix.to_string(), side_prefix.to_string());
                if uniform.is_none() {
                    uniform = Some(transform);
                } else if uniform.as_ref() != Some(&transform) {
                    uniform_ok = false;
                }
            },
            (_, None) => {},
            (None, Some(_)) => return None,
        }
    }

    let mut covered_end = chunk.end;
    while let Some(row) = rows.get(covered_end) {
        if row.base.is_some() {
            break;
        }
        let (None, Some(insertion)) = split_sides(row, side) else {
            break;
        };
        let (base_prefix, side_prefix) = match (&uniform, uniform_ok) {
            (Some(transform), true) => transform,
            _ => return None,
        };
        let delta = side_prefix.strip_prefix(base_prefix.as_str())?;
        lines.push(format!("{delta}{}", insertion.text));
        covered_end += 1;
    }

    Some(AutoResolution {
        lines,
        covered: chunk.start..covered_end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflicts(rows: &[MergeRow]) -> Vec<bool> {
        rows.iter().map(|r| r.conflict).collect()
    }

    fn text(side: &Option<ReviewSide>) -> Option<&str> {
        side.as_ref().map(|s| s.text.as_str())
    }

    #[test]
    fn disjoint_edits_never_conflict() {
        let rows = build_merge_rows("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nb\nc\nD\n", None);
        assert_eq!(conflicts(&rows), [false, false, false, false]);
        assert_eq!(text(&rows[1].ours), Some("B"));
        assert_eq!(text(&rows[1].theirs), Some("b"));
        assert_eq!(text(&rows[3].theirs), Some("D"));
    }

    #[test]
    fn same_line_divergent_edit_conflicts() {
        let rows = build_merge_rows(
            "1 shared header\n2 middle line\n3 shared footer\n",
            "1 shared header\n2 ours edit\n3 shared footer\n",
            "1 shared header\n2 theirs edit\n3 shared footer\n",
            None,
        );
        assert_eq!(conflicts(&rows), [false, true, false]);
        assert_eq!(text(&rows[1].base), Some("2 middle line"));
        assert_eq!(text(&rows[1].ours), Some("2 ours edit"));
        assert_eq!(text(&rows[1].theirs), Some("2 theirs edit"));
    }

    #[test]
    fn one_sided_insertion_interleaves() {
        let rows = build_merge_rows("a\nb\n", "a\nINSERT\nb\n", "a\nb\n", None);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            text(&rows[1].base),
            None,
            "an insertion has no ancestor line"
        );
        assert_eq!(text(&rows[1].ours), Some("INSERT"));
        assert_eq!(text(&rows[1].theirs), None);
        assert!(!rows[1].conflict);
    }

    #[test]
    fn pure_context_pairs_three_ways() {
        let rows = build_merge_rows("x\ny\nz\n", "x\ny\nz\n", "x\ny\nz\n", None);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| {
            r.base.is_some() && r.ours.is_some() && r.theirs.is_some() && !r.conflict
        }));
    }

    fn doc(ancestor: &str, ours: &str, theirs: &str) -> MergeDoc {
        MergeDoc::build(ancestor, ours, theirs, None)
    }

    fn chunk_ranges(chunks: &[ConflictChunk]) -> Vec<Range<usize>> {
        chunks.iter().map(|c| c.row_range.clone()).collect()
    }

    #[test]
    fn chunking_merges_adjacent_conflicts_and_splits_on_context() {
        let adjacent = doc("a\nb\nc\nd\n", "a\nB\nC\nd\n", "a\nX\nY\nd\n");
        assert_eq!(conflicts(&adjacent.rows), [false, true, true, false]);
        assert_eq!(adjacent.chunks.len(), 1);
        assert_eq!(adjacent.chunks[0].row_range, 1..3);

        let separated = doc("a\nb\nc\nd\ne\n", "a\nB\nc\nD\ne\n", "a\nX\nc\nY\ne\n");
        assert_eq!(
            conflicts(&separated.rows),
            [false, true, false, true, false]
        );
        assert_eq!(chunk_ranges(&separated.chunks), [1..2, 3..4]);
    }

    #[test]
    fn chunk_side_lines_read_each_side() {
        let merged = doc("a\nb1\nb2\nc\n", "a\nO1\nO2\nc\n", "a\nT1\nT2\nc\n");
        let chunk = &merged.chunks[0];
        assert_eq!(chunk.base_lines(&merged.rows), ["b1", "b2"]);
        assert_eq!(chunk.ours_lines(&merged.rows), ["O1", "O2"]);
        assert_eq!(chunk.theirs_lines(&merged.rows), ["T1", "T2"]);
    }

    #[test]
    fn initial_center_auto_takes_one_sided_edits() {
        let (center, ranges) = doc("a\nb\nc\n", "a\nB\nc\n", "a\nb\nC\n").initial_center_text();
        assert_eq!(center, "a\nB\nC\n");
        assert!(ranges.is_empty(), "no conflict chunks to anchor");
    }

    #[test]
    fn initial_center_emits_marker_block_with_byte_range() {
        let merged = doc("a\nb\nc\n", "a\nO\nc\n", "a\nT\nc\n");
        let (center, ranges) = merged.initial_center_text();
        assert_eq!(
            center,
            "a\n<<<<<<< ours\nO\n=======\nT\n>>>>>>> theirs\nc\n"
        );
        assert_eq!(ranges.len(), 1);
        assert_eq!(
            merged.chunks[0].marker_text(&merged.rows),
            center[ranges[0].clone()]
        );
    }

    #[test]
    fn assembly_orders_ours_before_theirs_by_row() {
        let merged = doc("a\nb1\nb2\nc\n", "a\nO1\nO2\nc\n", "a\nT1\nT2\nc\n");
        let chunk = &merged.chunks[0];
        assert_eq!(
            chunk.assembly_text(&merged.rows, &chunk.all_both()),
            "O1\nT1\nO2\nT2\n"
        );
        let mixed = [
            RowPick {
                ours: true,
                theirs: false,
            },
            RowPick {
                ours: false,
                theirs: true,
            },
        ];
        assert_eq!(chunk.assembly_text(&merged.rows, &mixed), "O1\nT2\n");
    }

    #[test]
    fn partial_text_shrinks_the_marker_block_as_rows_are_decided() {
        let merged = doc("a\nb1\nb2\nc\n", "a\nO1\nO2\nc\n", "a\nT1\nT2\nc\n");
        let chunk = &merged.chunks[0];
        let rows = &merged.rows;

        let one_decided = [
            RowPick {
                ours: true,
                theirs: false,
            },
            RowPick {
                ours: false,
                theirs: false,
            },
        ];
        assert_eq!(
            chunk.partial_text(rows, &one_decided),
            "O1\n<<<<<<< ours\nO2\n=======\nT2\n>>>>>>> theirs\n"
        );
        assert_eq!(
            chunk.classify(rows, &one_decided, &chunk.partial_text(rows, &one_decided)),
            ChunkState::Picked
        );

        let both_decided = [
            RowPick {
                ours: true,
                theirs: false,
            },
            RowPick {
                ours: false,
                theirs: true,
            },
        ];
        assert_eq!(
            chunk.partial_text(rows, &both_decided),
            chunk.assembly_text(rows, &both_decided),
            "every row decided drops the marker block"
        );
    }

    #[test]
    fn center_row_to_merge_row_maps_result_and_marker_lines() {
        let merged = doc("a\nb1\nb2\nc\n", "a\nO1\nO2\nc\n", "a\nT1\nT2\nc\n");
        let chunk = &merged.chunks[0];
        let rows = &merged.rows;
        let picks = [
            RowPick {
                ours: true,
                theirs: false,
            },
            RowPick {
                ours: false,
                theirs: false,
            },
        ];
        let at = |row| chunk.center_row_to_merge_row(rows, &picks, row);

        assert_eq!(at(0), Some((1, Side::Ours)), "decided result line");
        assert_eq!(at(1), None, "<<<<<<< delimiter");
        assert_eq!(at(2), Some((2, Side::Ours)), "ours marker-section line");
        assert_eq!(at(3), None, "======= delimiter");
        assert_eq!(at(4), Some((2, Side::Theirs)), "theirs marker-section line");
        assert_eq!(at(5), None, ">>>>>>> delimiter");
        assert_eq!(at(6), None, "past the rendering");
    }

    #[test]
    fn classify_round_trips_every_state() {
        let merged = doc("a\nb1\nb2\nc\n", "a\nO1\nO2\nc\n", "a\nT1\nT2\nc\n");
        let chunk = &merged.chunks[0];
        let rows = &merged.rows;
        let mixed = [
            RowPick {
                ours: true,
                theirs: false,
            },
            RowPick {
                ours: false,
                theirs: true,
            },
        ];
        let cls = |region: &str| chunk.classify(rows, &mixed, region);

        assert_eq!(cls(&chunk.marker_text(rows)), ChunkState::Unresolved);
        assert_eq!(
            cls(&chunk.assembly_text(rows, &chunk.all_ours())),
            ChunkState::Ours
        );
        assert_eq!(
            cls(&chunk.assembly_text(rows, &chunk.all_theirs())),
            ChunkState::Theirs
        );
        assert_eq!(
            cls(&chunk.assembly_text(rows, &chunk.all_both())),
            ChunkState::Both
        );
        assert_eq!(cls(&chunk.assembly_text(rows, &mixed)), ChunkState::Picked);
        assert_eq!(cls("hand edited\n"), ChunkState::Manual);
        assert_eq!(
            cls("<<<<<<< ours\nO1\nO2\n=======\nT1\nT2\n>>>>>>> theirs"),
            ChunkState::Unresolved,
            "trailing newline ignored"
        );
    }

    #[test]
    fn delete_edit_conflict_markers_have_empty_ours_section() {
        let merged = doc("a\nb\nc\n", "a\nc\n", "a\nB\nc\n");
        assert_eq!(conflicts(&merged.rows), [false, true, false]);
        let chunk = &merged.chunks[0];
        assert_eq!(chunk.ours_lines(&merged.rows), Vec::<&str>::new());
        assert_eq!(
            chunk.marker_text(&merged.rows),
            "<<<<<<< ours\n=======\nB\n>>>>>>> theirs\n"
        );
    }

    #[test]
    fn indent_resolves_when_one_side_only_reindents() {
        let merged = doc("a\nb\nc\n", "    a\n    b\n    c\n", "a\nB\nc\n");
        let (center, _) = merged.initial_center_text();
        assert_eq!(center, "    a\n    B\n    c\n");

        let chunk = &merged.chunks[0];
        assert!(chunk.auto.is_some());
        assert_eq!(
            chunk.classify(&merged.rows, &[], "    B\n"),
            ChunkState::AutoIndent
        );
    }

    #[test]
    fn indent_reindents_absorbed_content_side_insertion() {
        let merged = doc("a\nb\nc\n", "    a\n    b\n    c\n", "a\nB\nINS\nc\n");
        let (center, _) = merged.initial_center_text();
        assert_eq!(center, "    a\n    B\n    INS\n    c\n");

        let auto = merged.chunks[0].auto.as_ref().expect("auto-resolved");
        assert_eq!(auto.lines, ["    B", "    INS"]);
        assert_eq!(auto.covered, 1..3);
    }

    #[test]
    fn indent_stays_conflict_when_both_change_content() {
        let merged = doc("a\nb\nc\n", "a\nOURS\nc\n", "a\nTHEIRS\nc\n");
        assert!(merged.chunks[0].auto.is_none());
    }

    #[test]
    fn indent_stays_conflict_on_tab_space_conversion() {
        let merged = doc("    x\n", "\tx\n", "    Y\n");
        assert_eq!(conflicts(&merged.rows), [true]);
        assert!(merged.chunks[0].auto.is_none());
    }

    #[test]
    fn indent_resolves_symmetric_theirs_reindents() {
        let merged = doc("a\nb\nc\n", "a\nB\nc\n", "    a\n    b\n    c\n");
        let (center, _) = merged.initial_center_text();
        assert_eq!(center, "    a\n    B\n    c\n");
        assert!(merged.chunks[0].auto.is_some());
    }
}
