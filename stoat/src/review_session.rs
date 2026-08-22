use crate::{
    diff_map::BaseHighlights,
    review::{
        extract_review_hunks_changeset, line_byte_offsets, split_lines, ReviewFileInput,
        ReviewHunk, ReviewRow,
    },
};
use std::{collections::HashMap, ops::Range, sync::Arc};
use stoat_language::{structural_diff::TreeCache, Language};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ReviewChunkId(u32);

/// One hunk of a diffed file, with where it sits recorded on both sides.
///
/// The placement fields carry more than any one consumer needs. A commit
/// preview paints [`Self::hunk`] and numbers chunks by
/// [`Self::chunk_index_in_file`], and nothing today locates a chunk by line or
/// byte. They stay because a chunk that cannot say where it came from is not a
/// chunk, and the extraction that fills them is what the tests pin.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct ReviewChunk {
    pub id: ReviewChunkId,
    pub file_index: usize,
    pub chunk_index_in_file: usize,
    pub hunk: ReviewHunk,
    /// 0-based half-open row range in the buffer (RHS) text. Empty for
    /// pure-deletion chunks; a caller that locates a chunk by row falls back
    /// to [`Self::base_line_range`] in that case.
    pub buffer_line_range: Range<u32>,
    /// 0-based half-open row range in the base (LHS) text.
    pub base_line_range: Range<u32>,
    pub buffer_byte_range: Range<usize>,
    pub base_byte_range: Range<usize>,
}

#[derive(Clone)]
pub(crate) struct ReviewFile {
    pub rel_path: String,
    pub language: Option<Arc<Language>>,
    pub base_text: Arc<String>,
    pub buffer_text: Arc<String>,
    pub chunks: Vec<ReviewChunkId>,
    /// Tree-sitter spans for each side, per 0-based line, so a preview paints
    /// the same token colors the editor does.
    ///
    /// `None` until a preview build attaches them, and left `None` for a file
    /// with no language or when syntax highlighting is off. The paint falls
    /// back to untokenized text either way, which is what a syntax-off editor
    /// shows. Baked against the theme in force at build, so a theme switch
    /// drops the documents holding them.
    pub base_highlights: Option<Arc<BaseHighlights>>,
    pub buffer_highlights: Option<Arc<BaseHighlights>>,
}

/// A built diff, as the files it covers and the chunks they are cut into.
///
/// This is everything a reader of a diff needs and nothing more. The commit
/// picker, the commits view, and the background warm all read a diff without
/// staging anything in it, so a document carries no cursor, no per-chunk
/// status, and no editor.
#[derive(Default)]
pub(crate) struct DiffDocument {
    pub files: Vec<ReviewFile>,
    pub chunks: HashMap<ReviewChunkId, ReviewChunk>,
    /// Every chunk id in visit order, files in the order they were added.
    pub order: Vec<ReviewChunkId>,
    /// Base-side parse trees retained across the diffs this document builds,
    /// so a rebuild against an unmoved base reparses neither side.
    tree_cache: TreeCache,
    next_id: u32,
}

impl DiffDocument {
    /// Add one or more files to the session in a single cross-file
    /// structural-diff pass. Returns one chunk-id list per input in
    /// input order. Files that produce no hunks are still recorded
    /// (with an empty chunk list) so that subsequent file indices
    /// stay stable.
    pub(crate) fn add_files(&mut self, files: Vec<ReviewFileInput>) -> Vec<Vec<ReviewChunkId>> {
        let hunks_per_file = {
            let memo = self.tree_cache.clone();
            extract_review_hunks_changeset(&files, 3, Some(&memo))
        };
        self.add_files_with_hunks(files, hunks_per_file)
    }

    /// Add files that already have their hunks computed, skipping the
    /// changeset diff [`add_files`] runs. The streaming review scan uses
    /// this to reuse hunks it produced from prepared diffs, so a scanned
    /// file is never diffed a second time.
    pub(crate) fn add_files_with_hunks(
        &mut self,
        files: Vec<ReviewFileInput>,
        hunks_per_file: Vec<Vec<ReviewHunk>>,
    ) -> Vec<Vec<ReviewChunkId>> {
        let all_chunk_ids: Vec<Vec<ReviewChunkId>> = files
            .into_iter()
            .zip(hunks_per_file)
            .map(|(file, hunks)| self.push_file_with_hunks(file, hunks))
            .collect();
        all_chunk_ids
    }

    /// Append one already-diffed file's chunks with a stable file index,
    /// returning the ids allocated for its hunks.
    ///
    /// Leaves the cursor and version untouched so a caller adding a batch of
    /// files sets the cursor and bumps the version once at the end rather than
    /// per file.
    fn push_file_with_hunks(
        &mut self,
        file: ReviewFileInput,
        hunks: Vec<ReviewHunk>,
    ) -> Vec<ReviewChunkId> {
        let file_index = self.files.len();

        let base_offsets = line_byte_offsets(&split_lines(&file.base_text));
        let buffer_offsets = line_byte_offsets(&split_lines(&file.buffer_text));

        let mut chunk_ids: Vec<ReviewChunkId> = Vec::with_capacity(hunks.len());
        for (chunk_index_in_file, hunk) in hunks.into_iter().enumerate() {
            let id = self.alloc_id();
            let (base_line_range, buffer_line_range) = hunk_line_ranges(&hunk);
            let base_byte_range = lines_to_bytes(&base_offsets, &base_line_range);
            let buffer_byte_range = lines_to_bytes(&buffer_offsets, &buffer_line_range);

            self.chunks.insert(
                id,
                ReviewChunk {
                    id,
                    file_index,
                    chunk_index_in_file,
                    hunk,
                    buffer_line_range,
                    base_line_range,
                    buffer_byte_range,
                    base_byte_range,
                },
            );
            self.order.push(id);
            chunk_ids.push(id);
        }

        self.files.push(ReviewFile {
            rel_path: file.rel_path,
            language: file.language,
            base_text: file.base_text,
            buffer_text: file.buffer_text,
            chunks: chunk_ids.clone(),
            base_highlights: None,
            buffer_highlights: None,
        });

        chunk_ids
    }
    fn alloc_id(&mut self) -> ReviewChunkId {
        let id = ReviewChunkId(self.next_id);
        self.next_id += 1;
        id
    }
}

fn lines_to_bytes(offsets: &[(usize, usize)], lines: &Range<u32>) -> Range<usize> {
    if lines.start >= lines.end || offsets.is_empty() {
        return 0..0;
    }
    let start_idx = lines.start as usize;
    let end_idx = (lines.end as usize).min(offsets.len());
    if start_idx >= offsets.len() {
        return 0..0;
    }
    let start = offsets[start_idx].0;
    let end = offsets[end_idx.saturating_sub(1)].1;
    start..end
}

/// Returns the (base, buffer) 0-based half-open line ranges covered by the
/// changed rows of the hunk. Context rows are excluded because a chunk is
/// addressed by its *change*, not its display extent.
fn hunk_line_ranges(hunk: &ReviewHunk) -> (Range<u32>, Range<u32>) {
    let mut base_min: Option<u32> = None;
    let mut base_max: Option<u32> = None;
    let mut buf_min: Option<u32> = None;
    let mut buf_max: Option<u32> = None;

    for row in &hunk.rows {
        if let ReviewRow::Changed { left, right } = row {
            if let Some(l) = left {
                let v = l.line_num.saturating_sub(1);
                base_min = Some(base_min.map_or(v, |m| m.min(v)));
                base_max = Some(base_max.map_or(v + 1, |m| m.max(v + 1)));
            }
            if let Some(r) = right {
                let v = r.line_num.saturating_sub(1);
                buf_min = Some(buf_min.map_or(v, |m| m.min(v)));
                buf_max = Some(buf_max.map_or(v + 1, |m| m.max(v + 1)));
            }
        }
    }

    let base = match (base_min, base_max) {
        (Some(s), Some(e)) => s..e,
        _ => 0..0,
    };
    let buffer = match (buf_min, buf_max) {
        (Some(s), Some(e)) => s..e,
        _ => 0..0,
    };
    (base, buffer)
}

#[cfg(test)]
mod tests {
    use super::{DiffDocument, ReviewChunkId};
    use crate::review::{extract_review_hunks_changeset, ReviewFileInput};
    use std::{path::PathBuf, sync::Arc};

    fn input(path: &str, base: &str, buffer: &str) -> ReviewFileInput {
        ReviewFileInput {
            path: PathBuf::from(path),
            rel_path: path.to_string(),
            language: None,
            base_text: Arc::new(base.to_string()),
            buffer_text: Arc::new(buffer.to_string()),
        }
    }

    fn add(doc: &mut DiffDocument, path: &str, base: &str, buffer: &str) -> Vec<ReviewChunkId> {
        doc.add_files(vec![input(path, base, buffer)])
            .pop()
            .unwrap_or_default()
    }

    #[test]
    fn add_files_with_hunks_matches_add_files() {
        let files = || {
            vec![
                input("a.txt", "a\nb\nc\n", "a\nB\nc\n"),
                input("b.txt", "x\ny\n", "x\nY\n"),
            ]
        };

        let mut via_add = DiffDocument::default();
        let ids_add = via_add.add_files(files());

        let mut via_hunks = DiffDocument::default();
        let file_set = files();
        let precomputed = extract_review_hunks_changeset(&file_set, 3, None);
        let ids_hunks = via_hunks.add_files_with_hunks(file_set, precomputed);

        assert_eq!(
            ids_add, ids_hunks,
            "prebuilt hunks yield the same chunk ids"
        );
        assert_eq!(via_add.order, via_hunks.order);
    }

    #[test]
    fn line_and_byte_ranges_cover_changes() {
        let mut doc = DiffDocument::default();
        let ids = add(&mut doc, "a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        let chunk = &doc.chunks[&ids[0]];
        assert_eq!(chunk.base_line_range, 1..2);
        assert_eq!(chunk.buffer_line_range, 1..2);
        assert_eq!(chunk.base_byte_range, 2..5);
        assert_eq!(chunk.buffer_byte_range, 2..5);
    }

    #[test]
    fn pure_addition_has_empty_base_range() {
        let mut doc = DiffDocument::default();
        let ids = add(&mut doc, "a.txt", "a\nb\n", "a\nNEW\nb\n");
        let chunk = &doc.chunks[&ids[0]];
        assert_eq!(chunk.base_line_range, 0..0);
        assert_eq!(chunk.buffer_line_range, 1..2);
    }

    #[test]
    fn pure_deletion_has_empty_buffer_range() {
        let mut doc = DiffDocument::default();
        let ids = add(&mut doc, "a.txt", "a\nOLD\nb\n", "a\nb\n");
        let chunk = &doc.chunks[&ids[0]];
        assert_eq!(chunk.base_line_range, 1..2);
        assert_eq!(chunk.buffer_line_range, 0..0);
    }
}
