//! The diff overview, which shows every change against the diff base as one
//! side-by-side list of hunks.
//!
//! An overview row is a row of the commit preview's layout. Each chunk takes a
//! header row and then its diff rows, with files in document order. The row
//! model numbers those same rows. So a cursor row resolves to the file, the
//! chunk, and the buffer line that the paint puts on that row.
//!
//! The workspace builds the document an overview shows on the pool and keeps
//! the last one, so an overview paints at once while the next one builds.
//!
//! See also:
//! - [`render_commit_preview`](crate::render::commits::render_commit_preview) for the paint that
//!   lays out these rows.
//! - [`ensure_overview_doc`] for the build.

use crate::{
    action_handlers::{self, commits::PreviewHighlights},
    app::Stoat,
    buffer_registry::BufferRegistry,
    diff,
    diff_cache::{DiffCache, DiffCacheKey},
    host::{FsHost, GitHost, GitRepo},
    pane::View,
    review::{ReviewFileInput, ReviewHunk, ReviewRow, ReviewSide},
    review_session::{DiffDocument, ReviewChunk, ReviewChunkId},
    workspace::{self, diff::DiffBase, Workspace, WorkspaceId},
};
use slotmap::new_key_type;
use std::{
    collections::{hash_map::DefaultHasher, BTreeSet, HashMap},
    hash::{Hash, Hasher},
    io,
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
};
use stoat_language::{structural_diff::TreeCache, LanguageRegistry};
use stoat_scheduler::Task;
use stoat_text::Rope;

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

impl Default for OverviewState {
    /// An overview on its first row, in normal mode, that covers no view and
    /// waits on no landing.
    fn default() -> Self {
        Self {
            cursor: 0,
            scroll_top: 0,
            viewport_rows: 0,
            mode: "normal".to_string(),
            covered: None,
            covered_diff: false,
            pending_landing: None,
        }
    }
}

/// The state of a workspace that an overview document was built from.
///
/// A document is current while its stamp equals the workspace's stamp now.
/// `epoch` moves when the diff base or the git state changes, and `buffers`
/// moves with the text of every open buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OverviewStamp {
    epoch: u64,
    buffers: u64,
}

impl OverviewStamp {
    /// Read the stamp of `ws` as it stands.
    pub(crate) fn read(ws: &Workspace) -> Self {
        let mut hasher = DefaultHasher::new();
        for path in ws.buffers.open_paths() {
            let Some(id) = ws.buffers.id_for_path(&path) else {
                continue;
            };
            let Some(buffer) = ws.buffers.get(id) else {
                continue;
            };
            let version = buffer.read().expect("buffer poisoned").version();
            (id, version).hash(&mut hasher);
        }
        Self {
            epoch: ws.overview_epoch,
            buffers: hasher.finish(),
        }
    }
}

/// The last overview document a build finished, with the stamp it was built
/// from.
pub(crate) struct OverviewDoc {
    /// The document, or `None` when the build found no change to show.
    ///
    /// An empty build keeps its stamp all the same, so an unchanged tree does
    /// not build again.
    pub doc: Option<Arc<DiffDocument>>,
    pub stamp: OverviewStamp,
}

/// An overview document building on the pool, which [`pump_overview_build`]
/// stores when it lands.
pub(crate) struct PendingOverviewBuild {
    _task: Task<()>,
    rx: mpsc::Receiver<Option<DiffDocument>>,
    /// The workspace that asked for the build, which the document belongs to
    /// whatever workspace is active when it lands.
    workspace: WorkspaceId,
    stamp: OverviewStamp,
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

/// What an overview build reads, gathered on the run loop.
///
/// The build runs on the pool, so it carries owned copies of what it reads.
/// The hunk cache, the highlight cache, and the parse memo are the exceptions.
/// The build fills them, and each is shared behind its own lock.
struct BuildInputs {
    git_root: PathBuf,
    base: Option<DiffBase>,
    /// The text of every open buffer with a path. It wins over the disk, so an
    /// unsaved edit shows.
    open: HashMap<PathBuf, Rope>,
    langs: Arc<LanguageRegistry>,
    git_host: Arc<dyn GitHost>,
    fs_host: Arc<dyn FsHost>,
    cache: Arc<Mutex<DiffCache>>,
    highlights: PreviewHighlights,
    /// The base trees the last document parsed, so a rebuild reparses only
    /// the bases that moved.
    memo: TreeCache,
}

/// A chunk the overview shows, with the row its header takes.
struct PlacedChunk<'a> {
    file_index: usize,
    id: ReviewChunkId,
    chunk: &'a ReviewChunk,
    header: usize,
}

/// Start a build of the active workspace's overview document, unless one is
/// pending or the last one is current.
///
/// The build diffs every changed file against the base the per-file view uses,
/// on the pool. [`pump_overview_build`] stores what it finds on the workspace
/// that asked for it, so the last document paints while the next one builds.
pub(crate) fn ensure_overview_doc(stoat: &mut Stoat) {
    if stoat.pending_overview_build.is_some() {
        return;
    }
    let workspace = stoat.active_workspace;
    let ws = stoat.active_workspace();
    let stamp = OverviewStamp::read(ws);
    if ws
        .overview_doc
        .as_ref()
        .is_some_and(|built| built.stamp == stamp)
    {
        return;
    }

    let inputs = BuildInputs {
        git_root: ws.git_root.clone(),
        base: ws.diff_base().cloned(),
        open: open_texts(&ws.buffers),
        langs: stoat.language_registry.clone(),
        git_host: stoat.git_host.clone(),
        fs_host: stoat.fs_host.clone(),
        cache: stoat.diff_cache.clone(),
        highlights: PreviewHighlights::from_stoat(stoat),
        memo: ws
            .overview_doc
            .as_ref()
            .and_then(|built| built.doc.as_ref())
            .map(|doc| doc.tree_cache.clone())
            .unwrap_or_default(),
    };
    let redraw = stoat.redraw_notify.clone();
    let (tx, rx) = mpsc::channel();
    let task = stoat.executor.spawn_blocking(move || {
        let _ = tx.send(build(inputs));
        redraw.notify_one();
    });
    stoat.pending_overview_build = Some(PendingOverviewBuild {
        _task: task,
        rx,
        workspace,
        stamp,
    });
}

/// Store a landed overview build on the workspace that asked for it.
///
/// A build that found no change stores no document, which drops the document
/// before it. Returns whether a build landed, on the contract every pump in
/// [`Stoat::drive_pumps`] answers.
pub(crate) fn pump_overview_build(stoat: &mut Stoat) -> bool {
    let Some(pending) = stoat.pending_overview_build.take() else {
        return false;
    };
    let doc = match pending.rx.try_recv() {
        Ok(doc) => doc,
        Err(mpsc::TryRecvError::Empty) => {
            stoat.pending_overview_build = Some(pending);
            return false;
        },
        Err(mpsc::TryRecvError::Disconnected) => return false,
    };

    if let Some(ws) = stoat.workspaces.get_mut(pending.workspace) {
        ws.overview_doc = Some(OverviewDoc {
            doc: doc.map(Arc::new),
            stamp: pending.stamp,
        });
    }
    true
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

/// Build the overview document from `inputs`, on whatever thread calls.
///
/// `None` when the git root is in no repository, or when no file differs from
/// its base.
fn build(inputs: BuildInputs) -> Option<DiffDocument> {
    let repo = inputs.git_host.discover(&inputs.git_root)?;
    let workdir = repo.workdir()?;
    let files = file_inputs(&*repo, &workdir, &inputs);
    let hunks = file_hunks(&files, &inputs.cache, &inputs.memo);

    let mut doc = DiffDocument::default();
    doc.add_files_with_hunks(files, hunks);
    doc.tree_cache = inputs.memo;
    inputs.highlights.attach(&mut doc);
    (!doc.order.is_empty()).then_some(doc)
}

/// One input per changed file, in path order.
///
/// The paths are the files the repository lists as changed against the base,
/// with the untracked files left out, and every open buffer under the workdir,
/// so an unsaved edit shows. A file whose two texts agree gets no input.
///
/// An open buffer takes its own text over the disk's. A file gone from the disk
/// reads as empty, so its deletion shows. A file whose read fails for another
/// reason gets no input.
fn file_inputs(repo: &dyn GitRepo, workdir: &Path, inputs: &BuildInputs) -> Vec<ReviewFileInput> {
    let listed = match &inputs.base {
        Some(DiffBase::Rev { sha: Some(sha), .. }) => repo.changed_files_from(sha),
        _ => repo.changed_files(),
    };
    let paths: BTreeSet<PathBuf> = listed
        .into_iter()
        .filter(|file| !file.untracked)
        .map(|file| file.path)
        .chain(
            inputs
                .open
                .keys()
                .filter(|path| path.starts_with(workdir))
                .cloned(),
        )
        .collect();

    paths
        .into_iter()
        .filter_map(|path| {
            let base = workspace::diff::base_texts(repo, &path, inputs.base.as_ref())?.base;
            let buffer = match inputs.open.get(&path) {
                Some(rope) => rope.to_string(),
                None => match diff::read_utf8(&*inputs.fs_host, &path) {
                    Ok(text) => text,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
                    Err(_) => return None,
                },
            };
            if *base == buffer {
                return None;
            }
            Some(ReviewFileInput {
                rel_path: path
                    .strip_prefix(workdir)
                    .unwrap_or(&path)
                    .display()
                    .to_string(),
                language: inputs.langs.for_path(&path),
                base_text: base,
                buffer_text: Arc::new(buffer),
                path,
            })
        })
        .collect()
}

/// The hunks of each file in `files`, in the same order.
///
/// A file whose two texts the hunk cache holds reuses the cached hunks, so a
/// rebuild diffs only the files that moved. The rest go through one changeset
/// pass, which finds the moves between them, and their hunks enter the cache.
fn file_hunks(
    files: &[ReviewFileInput],
    cache: &Mutex<DiffCache>,
    memo: &TreeCache,
) -> Vec<Vec<ReviewHunk>> {
    let keys: Vec<DiffCacheKey> = files
        .iter()
        .map(|file| {
            action_handlers::review::diff_cache_key(
                &file.base_text,
                &file.buffer_text,
                file.language.as_ref(),
            )
        })
        .collect();
    let mut found: Vec<Option<Arc<Vec<ReviewHunk>>>> = {
        let mut cache = cache.lock().expect("diff_cache poisoned");
        keys.iter()
            .map(|key| cache.lookup(key).map(|(hunks, _)| hunks))
            .collect()
    };

    let misses: Vec<usize> = (0..files.len()).filter(|&i| found[i].is_none()).collect();
    if !misses.is_empty() {
        let missed: Vec<ReviewFileInput> = misses.iter().map(|&i| files[i].clone()).collect();
        let computed = diff::extract_review_hunks_changeset(&missed, 3, Some(memo));
        let mut cache = cache.lock().expect("diff_cache poisoned");
        for (&i, hunks) in misses.iter().zip(computed) {
            let hunks = Arc::new(hunks);
            cache.insert(keys[i].clone(), hunks.clone(), true);
            found[i] = Some(hunks);
        }
    }

    found
        .into_iter()
        .map(|hunks| hunks.map(Arc::unwrap_or_clone).unwrap_or_default())
        .collect()
}

/// The text of every open buffer with a path, keyed by the path.
fn open_texts(buffers: &BufferRegistry) -> HashMap<PathBuf, Rope> {
    buffers
        .open_paths()
        .into_iter()
        .filter_map(|path| {
            let buffer = buffers.get(buffers.id_for_path(&path)?)?;
            let rope = buffer.read().expect("buffer poisoned").rope().clone();
            Some((path, rope))
        })
        .collect()
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
    use super::{
        chunk_starts, ensure_overview_doc, row_at, row_for, target_of, OverviewRow, OverviewStamp,
        RowKind,
    };
    use crate::{
        action_handlers,
        host::FakeRepoBuilder,
        render::commits::preview_row_count,
        review::{ReviewFileInput, ReviewRow, ReviewSide},
        review_session::{DiffDocument, ReviewChunkId},
        test_harness::TestHarness,
        workspace::diff::DiffBase,
    };
    use std::{
        iter,
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat_action::{DiffAgainstHead, DiffAgainstIndex};

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

    /// The active workspace's overview document, once a build started now
    /// lands.
    fn built(h: &mut TestHarness) -> Option<Arc<DiffDocument>> {
        ensure_overview_doc(&mut h.stoat);
        h.settle();
        h.stoat
            .active_workspace()
            .overview_doc
            .as_ref()
            .and_then(|built| built.doc.clone())
    }

    /// Each file of `doc` as its path, its base text, its buffer text, and how
    /// many chunks it has.
    fn files(doc: &DiffDocument) -> Vec<(&str, &str, &str, usize)> {
        doc.files
            .iter()
            .map(|file| {
                (
                    file.rel_path.as_str(),
                    file.base_text.as_str(),
                    file.buffer_text.as_str(),
                    file.chunks.len(),
                )
            })
            .collect()
    }

    fn stamp(h: &TestHarness) -> OverviewStamp {
        OverviewStamp::read(h.stoat.active_workspace())
    }

    fn type_at_start(h: &mut TestHarness, text: &str) {
        h.type_keys("i");
        h.type_text(text);
        h.type_keys("esc");
    }

    /// The repository at /repo, with the harness's fake files under it.
    fn repo(h: &mut TestHarness) -> FakeRepoBuilder<'_> {
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git()
            .add_repo(PathBuf::from("/repo"))
            .with_fs(h.fake_fs())
    }

    /// An untracked file stays out, so a scratch file beside the work does not
    /// fill the overview.
    #[test]
    fn a_build_holds_every_tracked_change_in_path_order_with_its_colors() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario(
            "/repo",
            &[
                ("b.rs", "fn b() {}\n", "fn b() { 1 }\n"),
                ("a.rs", "fn a() {}\n", "fn a() { 2 }\n"),
            ],
        );
        repo(&mut h).untracked("z.txt");
        h.fake_fs()
            .insert_file(Path::new("/repo/z.txt"), b"scratch\n");

        let doc = built(&mut h).expect("two changed files build a document");
        let colored: Vec<bool> = doc
            .files
            .iter()
            .map(|file| file.buffer_highlights.is_some())
            .collect();
        assert_eq!(
            (files(&doc), colored),
            (
                vec![
                    ("a.rs", "fn a() {}\n", "fn a() { 2 }\n", 1),
                    ("b.rs", "fn b() {}\n", "fn b() { 1 }\n", 1),
                ],
                vec![true, true],
            ),
        );
    }

    /// The cache answers b.txt with no hunk, so a build that reads the cache
    /// shows b.txt with no chunk. A build that diffs it shows one chunk.
    #[test]
    fn a_rebuild_diffs_only_the_edited_file() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
        let cached = |h: &TestHarness, base: &str, buffer: &str| {
            h.stoat
                .diff_cache()
                .lock()
                .expect("diff_cache poisoned")
                .lookup(&action_handlers::review::diff_cache_key(base, buffer, None))
                .is_some()
        };
        h.stoat
            .diff_cache()
            .lock()
            .expect("diff_cache poisoned")
            .insert(
                action_handlers::review::diff_cache_key("b\n", "B\n", None),
                Arc::new(Vec::new()),
                true,
            );
        h.open_file(Path::new("/repo/a.txt"));

        let first = built(&mut h).expect("the first build");
        let a_cached = cached(&h, "a\n", "A\n");
        let before = stamp(&h);
        type_at_start(&mut h, "x");
        let moved = stamp(&h) != before;
        let second = built(&mut h).expect("the rebuild");
        assert_eq!(
            (files(&first), a_cached, moved, files(&second)),
            (
                vec![("a.txt", "a\n", "A\n", 1), ("b.txt", "b\n", "B\n", 0)],
                true,
                true,
                vec![("a.txt", "a\n", "xA\n", 1), ("b.txt", "b\n", "B\n", 0)],
            ),
        );
    }

    #[test]
    fn an_added_file_reads_as_every_line_added() {
        let mut h = TestHarness::with_size(80, 24);
        repo(&mut h).added("c.txt", "one\ntwo\n");

        let doc = built(&mut h).expect("an added file builds a document");
        let side = |side: Option<&ReviewSide>| side.map(|side| side.text.clone());
        let rows: Vec<(Option<String>, Option<String>)> = doc
            .order
            .iter()
            .flat_map(|id| doc.chunks[id].hunk.rows.iter())
            .map(|row| match row {
                ReviewRow::Context { left, right } => (side(Some(left)), side(Some(right))),
                ReviewRow::Changed { left, right } => (side(left.as_ref()), side(right.as_ref())),
            })
            .collect();
        assert_eq!(
            (files(&doc), rows),
            (
                vec![("c.txt", "", "one\ntwo\n", 1)],
                vec![
                    (None, Some("one".to_string())),
                    (None, Some("two".to_string())),
                ],
            ),
        );
    }

    /// A clean tree builds no document and keeps the build's stamp, so asking
    /// again builds nothing. An unsaved edit then shows, and the clean file
    /// open beside it stays out.
    #[test]
    fn an_unsaved_edit_to_a_clean_file_shows() {
        let mut h = TestHarness::with_size(80, 24);
        repo(&mut h)
            .head_file("d.txt", "same\n")
            .head_file("f.txt", "kept\n");
        h.fake_fs().insert_file(Path::new("/repo/d.txt"), b"same\n");
        h.fake_fs().insert_file(Path::new("/repo/f.txt"), b"kept\n");
        h.open_file(Path::new("/repo/f.txt"));
        h.open_file(Path::new("/repo/d.txt"));

        let clean = built(&mut h);
        ensure_overview_doc(&mut h.stoat);
        let rebuilt = h.stoat.pending_overview_build.is_some();
        type_at_start(&mut h, "new ");
        let edited = built(&mut h).expect("the unsaved edit builds a document");
        assert_eq!(
            (clean.is_none(), rebuilt, files(&edited)),
            (true, false, vec![("d.txt", "same\n", "new same\n", 1)]),
        );
    }

    #[test]
    fn a_base_switch_stales_the_document_and_rebuilds_against_the_new_base() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_index_scenario("/repo", &[("e.txt", "head\n", "index\n", "work\n")]);
        h.open_file(Path::new("/repo/e.txt"));

        action_handlers::dispatch(&mut h.stoat, &DiffAgainstIndex);
        let index = built(&mut h).expect("the index build");
        let before = stamp(&h);
        action_handlers::dispatch(&mut h.stoat, &DiffAgainstHead);
        let moved = stamp(&h) != before;
        let head = built(&mut h).expect("the head build");
        assert_eq!(
            (files(&index), moved, files(&head)),
            (
                vec![("e.txt", "index\n", "work\n", 1)],
                true,
                vec![("e.txt", "head\n", "work\n", 1)],
            ),
        );
    }

    /// A stage and a `.git` event move the base text where no buffer moves, so
    /// each stales the document.
    #[test]
    fn a_stage_and_a_git_event_each_stale_the_document() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");

        let first = stamp(&h);
        h.stoat
            .active_workspace_mut()
            .invalidate_diff(buffer_id, Path::new("/repo/a.txt"));
        let staged = stamp(&h);
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        let refreshed = stamp(&h);
        assert_eq!((first != staged, staged != refreshed), (true, true));
    }

    /// Under a revision base the overview lists what changed since that
    /// revision, which includes a file committed since and clean in the tree.
    #[test]
    fn a_revision_base_lists_what_changed_since_the_revision() {
        let mut h = TestHarness::with_size(80, 24);
        repo(&mut h)
            .head_file("committed.txt", "after\n")
            .commit("base0", &[("committed.txt", "before\n")]);
        h.fake_fs()
            .insert_file(Path::new("/repo/committed.txt"), b"after\n");
        h.stoat
            .active_workspace_mut()
            .set_diff_base(Some(DiffBase::named("base0", "base0".to_string())));

        let doc = built(&mut h).expect("the commit since the revision builds a document");
        assert_eq!(files(&doc), [("committed.txt", "before\n", "after\n", 1)]);
    }
}
