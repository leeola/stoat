//! The diff overview, which shows every change against the diff base as one
//! side-by-side list of hunks.
//!
//! An overview row is a row of the commit preview's layout. Each chunk takes a
//! header row and then its diff rows, with files in document order. The row
//! model numbers those same rows. So a cursor row resolves to the file, the
//! chunk, and the buffer line that the paint puts on that row.
//!
//! See also:
//! - [`render_commit_preview`](crate::render::commits::render_commit_preview) for the paint that
//!   lays out these rows.

use crate::{
    pane::View,
    review::{ReviewRow, ReviewSide},
    review_session::{DiffDocument, ReviewChunk, ReviewChunkId},
};
use slotmap::new_key_type;

new_key_type! {
    /// Key for one overview pane's [`OverviewState`].
    pub struct OverviewId;
}

/// One overview pane's cursor, scroll, and input mode, and the view the
/// overview replaced in that pane.
pub(crate) struct OverviewState {
    /// The row under the cursor, in the rows
    /// [`preview_row_count`](crate::render::commits::preview_row_count) counts.
    pub cursor: usize,
    /// The first row on screen, in the same rows as [`Self::cursor`].
    pub scroll_top: usize,
    /// Rows visible on the most recent paint. Zero until the first paint.
    pub viewport_rows: usize,
    /// The input mode while the overview's pane is focused.
    ///
    /// Each overview holds its own mode, as a commits list does, so a chord
    /// left pending on another pane never carries into the overview.
    pub mode: String,
    /// The view the overview replaced in its pane, which closing the overview
    /// puts back.
    pub covered: Option<View>,
    /// True when the covered editor showed the diff view, so closing the
    /// overview returns to the per-file diff it came from.
    pub covered_diff: bool,
    /// The `(rel_path, buffer_row)` the cursor moves onto once a document
    /// exists.
    ///
    /// An overview opens before its first document builds, so the editor line
    /// it opened from waits here.
    pub pending_landing: Option<(String, u32)>,
}

impl OverviewState {
    /// Scrolls the least distance that keeps [`Self::cursor`] on screen in a
    /// viewport of `rows` rows.
    ///
    /// A zero-row viewport, as before the first paint, leaves the scroll as it
    /// is.
    pub(crate) fn ensure_cursor_visible(&mut self, rows: usize) {
        if rows == 0 {
            return;
        }
        if self.cursor < self.scroll_top {
            self.scroll_top = self.cursor;
        } else if self.cursor >= self.scroll_top + rows {
            self.scroll_top = self.cursor + 1 - rows;
        }
    }
}

/// The chunk that one overview row belongs to, and the part of it the row
/// shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OverviewRow {
    pub file_index: usize,
    pub chunk: ReviewChunkId,
    pub kind: RowKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowKind {
    /// The chunk's header row, which names its file and its place in the file.
    Header,
    /// A diff row, as an index into the chunk's `hunk.rows`.
    Diff(usize),
}

/// A chunk the overview shows, with the row its header takes.
struct PlacedChunk<'a> {
    file_index: usize,
    id: ReviewChunkId,
    chunk: &'a ReviewChunk,
    header: usize,
}

/// The overview row at `row`, or `None` past the last row.
pub(crate) fn row_at(doc: &DiffDocument, row: usize) -> Option<OverviewRow> {
    let (placed, kind) = locate(doc, row)?;
    Some(OverviewRow {
        file_index: placed.file_index,
        chunk: placed.id,
        kind,
    })
}

/// The header row of every chunk, in row order.
pub(crate) fn chunk_starts(doc: &DiffDocument) -> Vec<usize> {
    placed_chunks(doc).map(|placed| placed.header).collect()
}

/// The overview row that shows line `buffer_row` of the file at `rel_path`.
///
/// A line inside a chunk lands on its own row. A line outside every chunk
/// lands on the header of the next chunk below it. A line below the file's
/// last chunk lands on the file's first chunk header. The answer is `None`
/// when the document holds no chunk of the file.
pub(crate) fn row_for(doc: &DiffDocument, rel_path: &str, buffer_row: u32) -> Option<usize> {
    let file_index = doc
        .files
        .iter()
        .position(|file| file.rel_path == rel_path)?;

    let mut first_header = None;
    let mut next_below = None;
    for placed in placed_chunks(doc).filter(|placed| placed.file_index == file_index) {
        let on_line = placed.chunk.hunk.rows.iter().position(|row| {
            right(row).is_some_and(|side| side.line_num.saturating_sub(1) == buffer_row)
        });
        if let Some(index) = on_line {
            return Some(placed.header + 1 + index);
        }

        first_header.get_or_insert(placed.header);
        if next_below.is_none() && buffer_start(placed.chunk) >= buffer_row {
            next_below = Some(placed.header);
        }
    }
    next_below.or(first_header)
}

/// The file and the buffer row that the overview row at `row` opens on.
///
/// A row with a buffer side opens on its own line. A deletion row has no
/// buffer side, so it opens on the next buffer line of its chunk, or on the
/// line above when the deletion ends the chunk. A header row opens where the
/// chunk's first change does. A chunk with no buffer line at all, from a file
/// emptied to nothing, opens on the file's first row.
pub(crate) fn target_of(doc: &DiffDocument, row: usize) -> Option<(String, u32)> {
    let (placed, kind) = locate(doc, row)?;
    let rows = &placed.chunk.hunk.rows;
    let index = match kind {
        RowKind::Header => rows
            .iter()
            .position(|row| matches!(row, ReviewRow::Changed { .. }))
            .unwrap_or(0),
        RowKind::Diff(index) => index,
    };

    let side = rows[index..]
        .iter()
        .find_map(right)
        .or_else(|| rows[..index].iter().rev().find_map(right));
    let rel_path = doc.files.get(placed.file_index)?.rel_path.clone();
    Some((
        rel_path,
        side.map_or(0, |side| side.line_num.saturating_sub(1)),
    ))
}

/// The chunk that holds `row`, and the part of the chunk the row shows.
fn locate(doc: &DiffDocument, row: usize) -> Option<(PlacedChunk<'_>, RowKind)> {
    let placed = placed_chunks(doc)
        .take_while(|placed| placed.header <= row)
        .last()?;
    let kind = match row - placed.header {
        0 => RowKind::Header,
        offset if offset <= placed.chunk.hunk.rows.len() => RowKind::Diff(offset - 1),
        _ => return None,
    };
    Some((placed, kind))
}

/// Every chunk in paint order, with the row its header takes.
///
/// The walk is the one
/// [`render_commit_preview`](crate::render::commits::render_commit_preview)
/// paints. Files go in document order and each file's chunks in its order, and
/// each chunk takes one header row and then its hunk's rows. A chunk id the
/// document does not hold paints nothing, so it takes no rows here either.
fn placed_chunks(doc: &DiffDocument) -> impl Iterator<Item = PlacedChunk<'_>> {
    doc.files
        .iter()
        .enumerate()
        .flat_map(move |(file_index, file)| {
            file.chunks
                .iter()
                .filter_map(move |&id| doc.chunks.get(&id).map(|chunk| (file_index, id, chunk)))
        })
        .scan(0, |next_header, (file_index, id, chunk)| {
            let header = *next_header;
            *next_header += 1 + chunk.hunk.rows.len();
            Some(PlacedChunk {
                file_index,
                id,
                chunk,
                header,
            })
        })
}

/// The buffer row where a chunk's change starts.
///
/// A pure deletion has no buffer rows, so its `buffer_line_range` is empty and
/// names no place. Its change sits on the row below its leading context.
fn buffer_start(chunk: &ReviewChunk) -> u32 {
    if !chunk.buffer_line_range.is_empty() {
        return chunk.buffer_line_range.start;
    }
    // A 1-based line number is the 0-based row below that line.
    chunk
        .hunk
        .rows
        .iter()
        .take_while(|row| matches!(row, ReviewRow::Context { .. }))
        .last()
        .and_then(right)
        .map_or(0, |side| side.line_num)
}

/// The buffer side of a diff row, which a deletion row lacks.
fn right(row: &ReviewRow) -> Option<&ReviewSide> {
    match row {
        ReviewRow::Context { right, .. } => Some(right),
        ReviewRow::Changed { right, .. } => right.as_ref(),
    }
}

#[cfg(test)]
mod tests {
    use super::{chunk_starts, row_at, row_for, target_of, OverviewRow, RowKind};
    use crate::{
        render::commits::preview_row_count,
        review::ReviewFileInput,
        review_session::{DiffDocument, ReviewChunkId},
    };
    use std::{iter, path::PathBuf, sync::Arc};

    /// Three files with no language, so the line diff cuts the chunks.
    ///
    /// a.txt gains a line near its top and loses its last line, far enough
    /// apart for two chunks. b.txt loses a line between two others. c.txt
    /// loses its only line.
    fn three_files() -> (DiffDocument, Vec<Vec<ReviewChunkId>>) {
        let input = |path: &str, base: &str, buffer: &str| ReviewFileInput {
            path: PathBuf::from(path),
            rel_path: path.to_string(),
            language: None,
            base_text: Arc::new(base.to_string()),
            buffer_text: Arc::new(buffer.to_string()),
        };
        let mut doc = DiffDocument::default();
        let ids = doc.add_files(vec![
            input(
                "a.txt",
                "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n",
                "1\nX\n2\n3\n4\n5\n6\n7\n8\n9\n",
            ),
            input("b.txt", "a\nOLD\nb\n", "a\nb\n"),
            input("c.txt", "gone\n", ""),
        ]);
        (doc, ids)
    }

    fn at(rel_path: &str, buffer_row: u32) -> Option<(String, u32)> {
        Some((rel_path.to_string(), buffer_row))
    }

    #[test]
    fn rows_number_each_chunk_header_then_its_diff_rows() {
        let (doc, ids) = three_files();
        let expected: Vec<_> = [
            (0, ids[0][0], 5),
            (0, ids[0][1], 4),
            (1, ids[1][0], 3),
            (2, ids[2][0], 1),
        ]
        .into_iter()
        .flat_map(|(file_index, chunk, diff_rows)| {
            let row = move |kind| {
                Some(OverviewRow {
                    file_index,
                    chunk,
                    kind,
                })
            };
            iter::once(row(RowKind::Header))
                .chain((0..diff_rows).map(move |i| row(RowKind::Diff(i))))
        })
        .chain([None])
        .collect();

        assert_eq!(
            (0..=preview_row_count(&doc))
                .map(|row| row_at(&doc, row))
                .collect::<Vec<_>>(),
            expected,
            "each chunk's header then its diff rows, and nothing past the paint"
        );
        assert_eq!(chunk_starts(&doc), [0, 6, 11, 15]);
    }

    #[test]
    fn row_for_lands_on_the_line_else_the_next_chunk_below_else_the_first() {
        let (doc, _) = three_files();
        assert_eq!(
            [
                ("a.txt", 1),
                ("a.txt", 5),
                ("a.txt", 10),
                ("a.txt", 20),
                ("b.txt", 1),
                ("c.txt", 0),
                ("d.txt", 0),
            ]
            .map(|(rel_path, buffer_row)| row_for(&doc, rel_path, buffer_row)),
            [Some(2), Some(6), Some(6), Some(0), Some(14), Some(15), None],
        );
    }

    #[test]
    fn target_of_opens_the_buffer_line_a_row_shows() {
        let (doc, _) = three_files();
        assert_eq!(
            [0, 1, 6, 10, 11, 13, 15, 16, 17].map(|row| target_of(&doc, row)),
            [
                at("a.txt", 1),
                at("a.txt", 0),
                at("a.txt", 9),
                at("a.txt", 9),
                at("b.txt", 1),
                at("b.txt", 1),
                at("c.txt", 0),
                at("c.txt", 0),
                None,
            ],
        );
    }
}
