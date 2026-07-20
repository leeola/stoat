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
                    chunks.push(ConflictChunk { row_range: s..i });
                    start = None;
                },
                _ => {},
            }
        }
        if let Some(s) = start {
            chunks.push(ConflictChunk {
                row_range: s..rows.len(),
            });
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
                text.push_str(&chunk.marker_text(&self.rows));
                ranges.push(start..text.len());
                i = chunk.row_range.end;
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
}

/// A maximal run of adjacent conflict rows within a [`MergeDoc`]. A non-conflict
/// row between conflicts separates one chunk from the next.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConflictChunk {
    pub(crate) row_range: Range<usize>,
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
    /// newlines ignored) against the candidate renderings in order: the marker
    /// block, the ours/theirs/both whole-side assemblies, then the assembly for
    /// `picks`. Anything else is [`ChunkState::Manual`].
    pub(crate) fn classify(
        &self,
        rows: &[MergeRow],
        picks: &[RowPick],
        region_text: &str,
    ) -> ChunkState {
        let target = region_text.trim_end_matches('\n');
        let matches = |candidate: &str| candidate.trim_end_matches('\n') == target;

        if matches(&self.marker_text(rows)) {
            ChunkState::Unresolved
        } else if matches(&self.assembly_text(rows, &self.all_ours())) {
            ChunkState::Ours
        } else if matches(&self.assembly_text(rows, &self.all_theirs())) {
            ChunkState::Theirs
        } else if matches(&self.assembly_text(rows, &self.all_both())) {
            ChunkState::Both
        } else if matches(&self.assembly_text(rows, picks)) {
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
}
