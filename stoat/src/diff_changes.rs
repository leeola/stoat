//! The changes view, which shows every change against the diff base as a
//! stack of per-file diff excerpts.
//!
//! Each changed file gets one editor in the per-file diff view, over the
//! workspace's own buffer of the file. The editor folds every unchanged run
//! that lies more than three rows from a hunk, so the excerpt shows the file's
//! hunks with their context. The stack paints the excerpts in path order, each
//! under a header row that names its file. The focused excerpt's editor is the
//! focused editor, so every editor key acts on the real file at the real line
//! and column.
//!
//! See also:
//! - [`pump_diff_changes_stack`] for the list of files, the folds, and the landings.
//! - [`layout_stack`] for the rows the paint lays the excerpts on.

use crate::{
    action_handlers::{review, view},
    app::Stoat,
    buffer::{BufferId, SharedBuffer},
    buffer_lifecycle,
    buffer_registry::{BufferRegistry, OpenOrigin},
    diff_map::DiffMap,
    display_map::{FoldSnapshot, InlayPoint},
    editor_state::{EditorId, EditorState},
    pane::View,
    render,
    workspace::{diff::DiffBase, Workspace},
};
use ratatui::layout::Rect;
use slotmap::{new_key_type, SlotMap};
use std::{
    collections::BTreeMap,
    mem,
    ops::Range,
    path::{Path, PathBuf},
};
use stoat_text::{Point, Rope};

/// Rows of unchanged context an excerpt shows on each side of a hunk.
const CONTEXT_ROWS: u32 = 3;

new_key_type! {
    /// Key for one changes view pane's [`DiffChangesState`].
    pub struct DiffChangesId;
}

/// One changes view pane's stack of excerpts, its scroll, and the view the
/// changes view replaced in that pane.
pub(crate) struct DiffChangesState {
    /// One excerpt per changed file, in path order.
    pub excerpts: Vec<Excerpt>,
    /// The index in [`Self::excerpts`] of the excerpt whose editor has focus.
    pub focused: usize,
    /// The first stack row on screen.
    ///
    /// A stack row is a header row or a display row of an excerpt, in the
    /// order [`layout_stack`] lays them out.
    pub scroll: usize,
    /// The workspace stamp the excerpts were listed at, or `None` before the
    /// first list.
    pub synced: Option<StackStamp>,
    /// Rows visible on the most recent paint. Zero until the first paint.
    pub viewport_rows: usize,
    /// The minimap strip the most recent paint reserved at the pane's right
    /// edge, or `None` where no per-pane strip shows.
    pub minimap_rect: Option<Rect>,
    /// The view the changes view replaced in its pane, which closing the
    /// changes view puts back.
    pub covered: Option<View>,
    /// True when the covered editor showed the diff view, so closing the
    /// changes view returns to the per-file diff it came from.
    pub covered_diff: bool,
    /// The place the cursor moves to once its file's excerpt is folded.
    ///
    /// A changes view opens before its excerpts exist, so the editor position
    /// it opened from waits here.
    pub pending_landing: Option<Landing>,
    /// The row of the view that the next paint puts the cursor's row on, in
    /// place of the least scroll that shows it.
    ///
    /// A landing sets it from the screen row the cursor held in the layout the
    /// changes view opened from. The paint applies it after it fits each
    /// excerpt to the width, so the rows it counts are the rows it paints.
    pub held_screen_row: Option<u16>,
    /// The focused excerpt's editor and its cursor as of the last paint.
    ///
    /// The paint scrolls to the cursor only when the cursor moved since, so a
    /// wheel scroll leaves the cursor off screen as an editor's wheel does,
    /// while a cursor move that no key made still comes into view.
    pub followed_cursor: Option<(EditorId, Point)>,
    /// The stack row of the focused excerpt's cursor as of the last emit to
    /// stoatty, or `None` before one.
    ///
    /// A selection step patches the page that the cursor left, so that page
    /// sheds the old selection even when no composite reads it.
    pub last_cursor_row: Option<usize>,
}

/// One changed file in a changes view's stack.
pub(crate) struct Excerpt {
    pub buffer_id: BufferId,
    /// The editor that shows the file in the diff view, which no pane shows.
    pub editor_id: EditorId,
    /// The file's path relative to the repository's working directory, which
    /// the header row shows and the stack sorts by.
    pub rel_path: String,
    /// The version of the buffer's diff map the folds were cut from, or `None`
    /// until the map lands.
    pub folded_version: Option<usize>,
    /// The stack row of the excerpt's header, which [`layout_stack`] writes.
    pub top: usize,
    /// Whether the stack opened the buffer, so the buffer closes with the
    /// excerpt. A buffer that was open before the excerpt stays open.
    pub owns_buffer: bool,
}

/// The editor position a changes view lands its cursor on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Landing {
    /// The buffer the position is in, whose diff map decides whether the file
    /// is still to join the list.
    pub buffer_id: BufferId,
    pub rel_path: String,
    pub row: u32,
    pub column: u32,
    /// The row of the view the cursor sat on in the layout the landing came
    /// from, so a flip keeps the line where the eyes are. `None` when the
    /// cursor sat above the view.
    pub screen_row: Option<u16>,
}

/// The state of a workspace that a changes view's excerpts were listed from.
///
/// A list is current while its stamp equals the workspace's stamp now.
/// `epoch` moves when the diff base, the git state, or the repository tally
/// changes. `maps` moves with the diff map of every open buffer, which
/// decides whether an unsaved edit puts its file in the list, and `paths`
/// moves when a buffer opens, closes, or changes its path.
///
/// Each part is a counter its owner moves, so a read walks no buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StackStamp {
    epoch: u64,
    maps: u64,
    paths: u64,
}

impl DiffChangesState {
    /// Scrolls the least distance that keeps stack row `row` on screen in a
    /// viewport of `rows` rows.
    ///
    /// A zero-row viewport, as before the first paint, leaves the scroll as it
    /// is.
    pub(crate) fn ensure_row_visible(&mut self, row: usize, rows: usize) {
        if rows == 0 {
            return;
        }
        if row < self.scroll {
            self.scroll = row;
        } else if row >= self.scroll + rows {
            self.scroll = row + 1 - rows;
        }
    }

    /// The excerpt whose editor has focus, or `None` on an empty stack.
    pub(crate) fn focused_excerpt(&self) -> Option<&Excerpt> {
        self.excerpts.get(self.focused)
    }
}

impl Default for DiffChangesState {
    /// An empty stack at its top, that covers no view and waits on no landing.
    fn default() -> Self {
        Self {
            excerpts: Vec::new(),
            focused: 0,
            scroll: 0,
            synced: None,
            viewport_rows: 0,
            minimap_rect: None,
            covered: None,
            covered_diff: false,
            pending_landing: None,
            held_screen_row: None,
            followed_cursor: None,
            last_cursor_row: None,
        }
    }
}

impl StackStamp {
    /// Read the stamp of `ws` as it stands.
    pub(crate) fn read(ws: &Workspace) -> Self {
        Self {
            epoch: ws.diff_changes_epoch,
            maps: ws.diff_maps_epoch(),
            paths: ws.buffers.paths_epoch(),
        }
    }
}

/// Bring every changes view of the active workspace in step with the
/// workspace, and report whether anything moved.
///
/// A moved stamp lists each stale stack's files anew. An excerpt whose file
/// left the list goes, and a file with no excerpt gets one. An excerpt whose
/// buffer's diff map moved refolds, and each waiting landing lands once its
/// excerpt is folded. The list reads the repository tally, which the pump
/// drives too, and the buffers a disposed changes view released close here.
/// Answers on the contract every pump in [`Stoat::drive_pumps`] answers.
pub(crate) fn pump_diff_changes_stack(stoat: &mut Stoat) -> bool {
    let released = mem::take(&mut stoat.active_workspace_mut().diff_changes_released);
    for &buffer_id in &released {
        buffer_lifecycle::close_released_buffer(stoat, buffer_id);
    }
    if stoat.active_workspace().diff_changes_views.is_empty() {
        return !released.is_empty();
    }

    let tallied = {
        let executor = stoat.executor.clone();
        let git_host = stoat.git_host.clone();
        let redraw = stoat.redraw_notify.clone();
        stoat
            .active_workspace_mut()
            .pump_tally(&executor, &git_host, &redraw)
    };
    let relisted = relist(stoat);
    let ws = stoat.active_workspace_mut();
    let refolded = refold(ws);
    let landed = land_pending(ws);
    !released.is_empty() || tallied || relisted || refolded || landed
}

/// Lay the excerpts of `state` out in stack rows, and answer the total.
///
/// Each excerpt takes a header row and then its display rows, and
/// [`Excerpt::top`] records its header's row. An excerpt that waits on its
/// diff map takes its header row alone, so the stack never shows a whole
/// unfolded file.
pub(crate) fn layout_stack(
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
) -> usize {
    let mut row = 0;
    for excerpt in &mut state.excerpts {
        excerpt.top = row;
        row += 1 + excerpt_rows(excerpt, editors);
    }
    row
}

/// The display rows of `excerpt`'s editor that the stack's window shows, as of
/// the last [`layout_stack`], or `None` when the window shows none of them.
///
/// `rows` is the excerpt's display row count, from [`excerpt_rows`]. The
/// header row above them is no row of the excerpt's editor.
pub(crate) fn shown_excerpt_rows(
    state: &DiffChangesState,
    excerpt: &Excerpt,
    rows: usize,
) -> Option<Range<u32>> {
    let first = excerpt.top + 1;
    let start = state.scroll.max(first);
    let end = (state.scroll + state.viewport_rows).min(first + rows);
    (start < end).then(|| (start - first) as u32..(end - first) as u32)
}

/// The display rows `excerpt` shows, zero while it waits on its diff map.
///
/// The count is current with every edit, fold, and wrap, and reads with no
/// snapshot clone while none moved, so a layout per frame pays the version
/// checks alone.
pub(crate) fn excerpt_rows(
    excerpt: &Excerpt,
    editors: &mut SlotMap<EditorId, EditorState>,
) -> usize {
    if excerpt.folded_version.is_none() {
        return 0;
    }
    editors
        .get_mut(excerpt.editor_id)
        .map_or(0, |editor| editor.display_map.line_count() as usize)
}

/// The stack row of the focused excerpt's cursor, as of the last
/// [`layout_stack`].
///
/// `None` on an empty stack, and while the focused excerpt waits on its diff
/// map.
pub(crate) fn cursor_stack_row(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
) -> Option<usize> {
    let excerpt = state.focused_excerpt()?;
    excerpt.folded_version?;
    let editor = editors.get_mut(excerpt.editor_id)?;
    Some(excerpt.top + 1 + view::cursor_display_row(editor) as usize)
}

/// Focus the excerpt that stack row `row` falls in and put its cursor at the
/// start of the row, as of the last [`layout_stack`], and report whether the
/// row held one.
///
/// A header row puts the cursor on its excerpt's first row, and a row past the
/// stack on the last row.
pub(crate) fn select_row(
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    row: usize,
) -> bool {
    let Some((index, display_row)) = excerpt_at(state, editors, row) else {
        return false;
    };
    let Some(editor) = editors.get_mut(state.excerpts[index].editor_id) else {
        return false;
    };
    let Some(point) = editor
        .display_map
        .snapshot()
        .row_start_at_or_after(display_row)
    else {
        return false;
    };
    place_cursor(editor, point);
    state.focused = index;
    true
}

/// Focus the excerpt that stack row `row` falls in and put its cursor on the
/// cell at screen column `column` of the row, as a press in an editor does,
/// and report whether the row held one.
///
/// The column maps against the excerpt's text column as of its last paint, so
/// a press left of the text lands at the start of the row. A header row puts
/// the cursor at the start of its excerpt's first row, and a row that shows
/// no buffer line, as a base row spliced in for a removal, at the start of the
/// first row below it that does.
pub(crate) fn press_cell(
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    row: usize,
    column: u16,
) -> bool {
    let Some((index, display_row)) = excerpt_at(state, editors, row) else {
        return false;
    };
    let header = row == state.excerpts[index].top;
    let Some(editor) = editors.get_mut(state.excerpts[index].editor_id) else {
        return false;
    };

    let snapshot = editor.display_map.snapshot();
    let cell = editor
        .text_rect
        .filter(|_| !header)
        .and_then(|text| {
            let column = column.saturating_sub(text.x);
            render::editor::display_cell_to_offset(&snapshot, display_row, 0, column, 0)
        })
        .map(|offset| snapshot.buffer_snapshot().rope().offset_to_point(offset));
    let Some(point) = cell.or_else(|| snapshot.row_start_at_or_after(display_row)) else {
        return false;
    };
    place_cursor(editor, point);
    state.focused = index;
    true
}

/// The focused excerpt's editor and its cursor's buffer position.
///
/// `None` on an empty stack.
pub(crate) fn focused_cursor(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
) -> Option<(EditorId, Point)> {
    let editor_id = state.focused_excerpt()?.editor_id;
    let editor = editors.get_mut(editor_id)?;
    Some((editor_id, cursor_point(editor)))
}

/// The excerpt that stack row `row` falls in, as an index, and the display row
/// of that excerpt the row shows, as of the last [`layout_stack`].
///
/// A header row answers its excerpt's first display row, and a row past the
/// stack answers the last row. `None` on a stack with no row to show.
fn excerpt_at(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    row: usize,
) -> Option<(usize, u32)> {
    let index = state
        .excerpts
        .iter()
        .rposition(|excerpt| excerpt.top <= row)?;
    let excerpt = &state.excerpts[index];
    let rows = excerpt_rows(excerpt, editors);
    if rows == 0 {
        return None;
    }
    let display_row = row.saturating_sub(excerpt.top + 1).min(rows - 1);
    Some((index, display_row as u32))
}

/// Scroll the focused changes view so its cursor sits inside the scrolloff
/// band, as an editor follows its cursor, and report whether the scroll moved.
///
/// The band is `scrolloff` rows at each edge, capped to half the viewport, and
/// the scroll stops at the stack's last page.
pub(crate) fn follow_cursor(stoat: &mut Stoat, scrolloff: u32) -> bool {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return false;
    };
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let Some(state) = diff_changes_views.get_mut(id) else {
        return false;
    };
    let total = layout_stack(state, editors);
    let Some(row) = cursor_stack_row(state, editors) else {
        return false;
    };

    let viewport = state.viewport_rows.max(1);
    let scrolloff = scrolloff as usize;
    let top = scrolloff.min(viewport.saturating_sub(1) / 2);
    let bottom = scrolloff.min(viewport / 2);
    let before = state.scroll;
    if row < state.scroll + top {
        state.scroll = row.saturating_sub(top);
    } else if row + bottom >= state.scroll + viewport {
        state.scroll = (row + bottom + 1)
            .saturating_sub(viewport)
            .min(total.saturating_sub(viewport));
    }
    state.scroll != before
}

/// Remove `excerpts`' editors, and close each buffer the stack opened when
/// nothing else needs it.
pub(crate) fn release_excerpts(stoat: &mut Stoat, excerpts: Vec<Excerpt>) {
    for excerpt in excerpts {
        drop_excerpt(stoat, excerpt);
    }
}

/// Remove `excerpts`' editors from `ws`, and queue each buffer the stack
/// opened for [`pump_diff_changes_stack`] to close.
///
/// For a caller that holds the workspace alone, since a buffer's close reaches
/// the language servers.
pub(crate) fn dispose_excerpts(ws: &mut Workspace, excerpts: Vec<Excerpt>) {
    for excerpt in excerpts {
        ws.editors.remove(excerpt.editor_id);
        if excerpt.owns_buffer {
            ws.diff_changes_released.push(excerpt.buffer_id);
        }
    }
}

/// Put `editor`'s one cursor on `point`, held inside the buffer.
pub(crate) fn place_cursor(editor: &mut EditorState, point: Point) {
    let snapshot = editor.display_map.snapshot();
    let buffer = snapshot.buffer_snapshot();
    let rope = buffer.rope();
    let row = point.row.min(rope.max_point().row);
    let column = point.column.min(rope.line_len(row));
    let offset = rope.point_to_offset(Point::new(row, column));
    editor.selections.set_block_cursor(offset, buffer);
}

/// The buffer position of `editor`'s cursor.
pub(crate) fn cursor_point(editor: &mut EditorState) -> Point {
    let snapshot = editor.display_map.snapshot();
    let buffer = snapshot.buffer_snapshot();
    let selection = editor.selections.newest_anchor();
    let offset = stoat_text::cursor_offset(
        buffer.rope(),
        buffer.resolve_anchor(&selection.tail()),
        buffer.resolve_anchor(&selection.head()),
    );
    buffer.rope().offset_to_point(offset)
}

/// The buffer row each hunk of `editor`'s diff map starts on, in buffer order.
pub(crate) fn hunk_starts(editor: &mut EditorState) -> Vec<u32> {
    let snapshot = editor.display_map.snapshot();
    snapshot.diff_map().map_or_else(Vec::new, |map| {
        map.hunks().map(|hunk| hunk.buffer_start_line).collect()
    })
}

/// The rows `editor`'s folds leave showing, from its buffer's diff map.
pub(crate) fn kept_rows(editor: &mut EditorState) -> Vec<Range<u32>> {
    let snapshot = editor.display_map.snapshot();
    let line_count = snapshot.buffer_snapshot().rope().max_point().row + 1;
    snapshot
        .diff_map()
        .map_or_else(Vec::new, |map| kept_runs(map, line_count))
}

/// The row a landing on `row` takes in an excerpt that shows `kept`.
///
/// A row the excerpt shows stays. A row a fold hides moves to the first row of
/// the next shown run below it, and below the last run to the first row of
/// the first run.
pub(crate) fn landing_row(kept: &[Range<u32>], row: u32) -> u32 {
    if kept.iter().any(|run| run.contains(&row)) {
        return row;
    }
    kept.iter()
        .find(|run| run.start > row)
        .or(kept.first())
        .map_or(row, |run| run.start)
}

/// List the changed files anew for every changes view whose list is stale, and
/// report whether any was.
fn relist(stoat: &mut Stoat) -> bool {
    let stale: Vec<DiffChangesId> = {
        let ws = stoat.active_workspace();
        let stamp = StackStamp::read(ws);
        ws.diff_changes_views
            .iter()
            .filter(|(_, state)| state.synced != Some(stamp))
            .map(|(id, _)| id)
            .collect()
    };
    if stale.is_empty() {
        return false;
    }

    stoat.sync_repo_root();
    let listed = changed_files(stoat);
    for &id in &stale {
        sync_excerpts(stoat, id, &listed);
    }

    // Read after the sync, since an excerpt that opens a buffer moves the
    // stamp.
    let ws = stoat.active_workspace_mut();
    let stamp = StackStamp::read(ws);
    for id in stale {
        if let Some(state) = ws.diff_changes_views.get_mut(id) {
            state.synced = Some(stamp);
        }
    }
    true
}

/// The files a changes view lists, keyed by their path relative to the working
/// directory, so the keys run in path order.
///
/// The repository's tally names the files changed against the base: the files
/// with an unstaged hunk under the index base, and the files that differ from
/// HEAD under any other base. Under a revision base the files changed since
/// the revision take the tally's place. An untracked file stays out, so a
/// scratch file beside the work does not fill the stack.
///
/// Every open buffer under the working directory whose diff map holds a hunk
/// joins the list, so an unsaved edit shows. Under the index base only an
/// unstaged hunk counts, as the tally counts.
///
/// Outside a repository the list is empty. The working directory is the
/// workspace's resolved [`Workspace::repo_root`], so a list opens no
/// repository.
fn changed_files(stoat: &Stoat) -> BTreeMap<String, PathBuf> {
    let ws = stoat.active_workspace();
    let Some(workdir) = ws.repo_root.clone() else {
        return BTreeMap::new();
    };
    let index_base = ws.diff_base().is_none();
    let listed: Vec<PathBuf> = match ws.diff_base() {
        Some(DiffBase::Rev { sha: Some(_), .. }) => ws
            .rev_changed_files()
            .map(<[PathBuf]>::to_vec)
            .unwrap_or_default(),
        _ => {
            let totals = match index_base {
                true => ws.repo_unstaged_hunk_totals(),
                false => ws.repo_hunk_totals(),
            };
            let untracked = ws.repo_untracked_files().unwrap_or_default();
            totals
                .unwrap_or_default()
                .iter()
                .filter(|(path, count)| *count > 0 && untracked.binary_search(path).is_err())
                .map(|(path, _)| workdir.join(path))
                .collect()
        },
    };
    let open = ws.buffers.open_paths().into_iter().filter(|path| {
        path.starts_with(&workdir)
            && ws
                .buffers
                .id_for_path(path)
                .and_then(|id| ws.buffers.get(id))
                .is_some_and(|buffer| {
                    let buffer = buffer.read().expect("buffer poisoned");
                    buffer
                        .diff_map
                        .as_ref()
                        .is_some_and(|map| map.hunks().any(|hunk| !index_base || !hunk.staged()))
                })
    });

    listed
        .into_iter()
        .chain(open)
        .filter_map(|path| {
            let rel_path = path.strip_prefix(&workdir).ok()?.display().to_string();
            Some((rel_path, path))
        })
        .collect()
}

/// Bring the excerpts of changes view `id` in step with `listed`.
///
/// An excerpt whose file left the list goes, as [`drop_excerpt`] drops it. A
/// listed file with no excerpt gets one. The excerpts sort by path, and the
/// focus stays on its file while the file stays listed.
fn sync_excerpts(stoat: &mut Stoat, id: DiffChangesId, listed: &BTreeMap<String, PathBuf>) {
    let Some(state) = stoat.active_workspace_mut().diff_changes_views.get_mut(id) else {
        return;
    };
    let focused_path = state
        .focused_excerpt()
        .map(|excerpt| excerpt.rel_path.clone());
    let (kept, gone): (Vec<Excerpt>, Vec<Excerpt>) = mem::take(&mut state.excerpts)
        .into_iter()
        .partition(|excerpt| listed.contains_key(&excerpt.rel_path));
    state.excerpts = kept;
    release_excerpts(stoat, gone);

    let missing: Vec<(&String, &PathBuf)> = {
        let held = stoat
            .active_workspace()
            .diff_changes_views
            .get(id)
            .map(|state| &state.excerpts[..])
            .unwrap_or_default();
        listed
            .iter()
            .filter(|(rel_path, _)| !held.iter().any(|excerpt| &excerpt.rel_path == *rel_path))
            .collect()
    };
    for (rel_path, path) in missing {
        let Some(excerpt) = open_excerpt(stoat, id, rel_path.clone(), path) else {
            continue;
        };
        match stoat.active_workspace_mut().diff_changes_views.get_mut(id) {
            Some(state) => state.excerpts.push(excerpt),
            None => drop_excerpt(stoat, excerpt),
        }
    }

    let Some(state) = stoat.active_workspace_mut().diff_changes_views.get_mut(id) else {
        return;
    };
    state.excerpts.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    state.focused = focused_path
        .and_then(|path| {
            state
                .excerpts
                .iter()
                .position(|excerpt| excerpt.rel_path == path)
        })
        .unwrap_or(state.focused)
        .min(state.excerpts.len().saturating_sub(1));
}

/// An excerpt of changes view `id` over the file at `path`, in the diff view,
/// with its diff map wanted.
///
/// `None` when the file reads as no buffer, as an image or an unreadable file
/// does. `None` too for a large file not yet open, which reads on the pool,
/// and [`adopt_excerpt`] adds its excerpt once the read lands.
fn open_excerpt(
    stoat: &mut Stoat,
    id: DiffChangesId,
    rel_path: String,
    path: &Path,
) -> Option<Excerpt> {
    let workspace = stoat.active_workspace;
    let owns_buffer = stoat.active_workspace().buffers.id_for_path(path).is_none();
    if owns_buffer && buffer_lifecycle::defer_excerpt_open(stoat, path, id, &rel_path) {
        return None;
    }
    let (buffer_id, buffer) =
        buffer_lifecycle::open_buffer_unshown(stoat, workspace, path, OpenOrigin::Visited)?;
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let mut editor = ws.seeded_editor(buffer_id, buffer, executor);
    editor.set_diff_view(true);
    let editor_id = ws.editors.insert(editor);
    review::ensure_diff_map(stoat, buffer_id, None);
    Some(Excerpt {
        buffer_id,
        editor_id,
        rel_path,
        folded_version: None,
        top: 0,
        owns_buffer,
    })
}

/// Add the excerpt of the file at `rel_path` to changes view `id` of the
/// active workspace, in path order.
///
/// `buffer_id` is the buffer a read on the pool registered for the file. The
/// stack owns it, since the read opened it for the excerpt, and the focus
/// stays on its file. A view that is gone, or that already holds an excerpt of
/// the file, takes nothing, and the buffer closes when nothing else needs it.
pub(crate) fn adopt_excerpt(
    stoat: &mut Stoat,
    id: DiffChangesId,
    rel_path: String,
    buffer_id: BufferId,
    buffer: SharedBuffer,
) {
    let held = stoat
        .active_workspace()
        .diff_changes_views
        .get(id)
        .map(|state| state.excerpts.iter().any(|e| e.rel_path == rel_path));
    if held != Some(false) {
        buffer_lifecycle::close_released_buffer(stoat, buffer_id);
        return;
    }

    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let mut editor = ws.seeded_editor(buffer_id, buffer, executor);
    editor.set_diff_view(true);
    let editor_id = ws.editors.insert(editor);
    review::ensure_diff_map(stoat, buffer_id, None);

    let Some(state) = stoat.active_workspace_mut().diff_changes_views.get_mut(id) else {
        return;
    };
    let focused_path = state
        .focused_excerpt()
        .map(|excerpt| excerpt.rel_path.clone());
    let at = state
        .excerpts
        .partition_point(|excerpt| excerpt.rel_path < rel_path);
    state.excerpts.insert(
        at,
        Excerpt {
            buffer_id,
            editor_id,
            rel_path,
            folded_version: None,
            top: 0,
            owns_buffer: true,
        },
    );
    state.focused = focused_path
        .and_then(|path| {
            state
                .excerpts
                .iter()
                .position(|excerpt| excerpt.rel_path == path)
        })
        .unwrap_or(state.focused);
}

/// Remove `excerpt`'s editor, and close its buffer when the stack opened it
/// and nothing else needs it.
fn drop_excerpt(stoat: &mut Stoat, excerpt: Excerpt) {
    stoat
        .active_workspace_mut()
        .editors
        .remove(excerpt.editor_id);
    if excerpt.owns_buffer {
        buffer_lifecycle::close_released_buffer(stoat, excerpt.buffer_id);
    }
}

/// Cut the folds of every excerpt whose buffer's diff map moved since its
/// folds were cut, and report whether any did.
///
/// An excerpt cut for the first time puts its cursor on a row it shows, by
/// [`landing_row`], unless a landing waits on its file.
fn refold(ws: &mut Workspace) -> bool {
    let Workspace {
        diff_changes_views,
        editors,
        buffers,
        ..
    } = ws;
    let mut any = false;
    for state in diff_changes_views.values_mut() {
        for excerpt in &mut state.excerpts {
            let Some(version) = map_version(buffers, excerpt.buffer_id) else {
                continue;
            };
            if excerpt.folded_version == Some(version) {
                continue;
            }
            let Some(editor) = editors.get_mut(excerpt.editor_id) else {
                continue;
            };
            let Some((folded, kept)) = fold_excerpt(editor) else {
                continue;
            };
            let first = excerpt.folded_version.is_none();
            excerpt.folded_version = Some(folded);
            any = true;

            let landing_waits = state
                .pending_landing
                .as_ref()
                .is_some_and(|landing| landing.rel_path == excerpt.rel_path);
            if first && !landing_waits {
                let row = cursor_point(editor).row;
                place_cursor(editor, Point::new(landing_row(&kept, row), 0));
            }
        }
    }
    any
}

/// Land the cursor of each changes view of `ws` that waits on a landing, and
/// report whether any landed.
///
/// A landing waits until the excerpt of its file is folded. A landing whose
/// file has no excerpt waits until the list is settled for the file. The list
/// is settled when it is current, the tally is in, and the file's buffer has a
/// current diff map. The landing then lands on the first excerpt's first row.
fn land_pending(ws: &mut Workspace) -> bool {
    let stamp = StackStamp::read(ws);
    let tally_pending = ws.tally_pending();
    let waiting: Vec<(DiffChangesId, bool)> = ws
        .diff_changes_views
        .iter()
        .filter_map(|(id, state)| {
            let buffer_id = state.pending_landing.as_ref()?.buffer_id;
            let map_settled = ws.buffers.get(buffer_id).is_none()
                || ws.diff_map_current(buffer_id)
                || ws.hunks_installed(buffer_id);
            Some((
                id,
                state.synced == Some(stamp) && !tally_pending && map_settled,
            ))
        })
        .collect();

    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let mut landed = false;
    for (id, settled) in waiting {
        if let Some(state) = diff_changes_views.get_mut(id) {
            landed |= land(state, editors, settled);
        }
    }
    landed
}

/// Land `state`'s waiting landing if its excerpt is ready, and report whether
/// it landed.
///
/// The landing focuses the excerpt of its file and puts the cursor on its row
/// and column, or on the row [`landing_row`] picks when a fold hides that row.
/// A file with no excerpt lands, once `settled`, on the first excerpt's first
/// row, and a stack with no excerpt drops the landing.
///
/// A landing on its own row holds its screen row for the next paint. A landing
/// that a fold or a missing excerpt moved shows another line, so the paint
/// scrolls the least distance instead.
fn land(
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    settled: bool,
) -> bool {
    let Some(landing) = state.pending_landing.clone() else {
        return false;
    };
    let target = state
        .excerpts
        .iter()
        .position(|excerpt| excerpt.rel_path == landing.rel_path);
    let (index, row, column, screen_row) = match target {
        Some(index) => (index, landing.row, landing.column, landing.screen_row),
        None if !settled => return false,
        None => (0, 0, 0, None),
    };
    let Some(excerpt) = state.excerpts.get(index) else {
        state.pending_landing = None;
        return true;
    };
    if excerpt.folded_version.is_none() {
        return false;
    }
    let Some(editor) = editors.get_mut(excerpt.editor_id) else {
        return false;
    };

    let shown = landing_row(&kept_rows(editor), row);
    let column = if shown == row { column } else { 0 };
    place_cursor(editor, Point::new(shown, column));
    state.focused = index;
    state.pending_landing = None;
    state.held_screen_row = screen_row.filter(|_| shown == row);
    true
}

/// Fold every unchanged run of `editor`'s buffer that lies more than
/// [`CONTEXT_ROWS`] rows from a hunk, and answer the diff map version the folds
/// were cut from with the rows left showing.
///
/// A fold that already hides a wanted gap stays. Only the gaps that opened or
/// closed since the last cut change the display map, so a landing that moves no
/// gap changes nothing.
///
/// `None`, with nothing folded, while the buffer has no diff map.
fn fold_excerpt(editor: &mut EditorState) -> Option<(usize, Vec<Range<u32>>)> {
    let (version, kept, stale, fresh) = {
        let snapshot = editor.display_map.snapshot();
        let map = snapshot.diff_map()?;
        let rope = snapshot.buffer_snapshot().rope();
        let line_count = rope.max_point().row + 1;
        let kept = kept_runs(map, line_count);
        let wanted: Vec<Range<Point>> = gaps(&kept, line_count)
            .into_iter()
            .map(|gap| fold_range(rope, gap))
            .collect();

        // The folds sit where their anchors carried them across every edit
        // since their cut, so a gap whose rows moved still finds its fold.
        let held = folded_ranges(snapshot.fold_snapshot());
        let stale: Vec<Range<Point>> = held
            .iter()
            .filter(|fold| !holds(&wanted, fold))
            .cloned()
            .collect();
        let fresh: Vec<Range<Point>> = wanted
            .into_iter()
            .filter(|fold| !holds(&held, fold))
            .collect();
        (map.version(), kept, stale, fresh)
    };
    if !stale.is_empty() {
        editor.display_map.unfold(stale);
    }
    if !fresh.is_empty() {
        editor.display_map.fold(fresh);
    }
    Some((version, kept))
}

/// The buffer range each fold of `folds` hides, sorted by start and then end.
fn folded_ranges(folds: &FoldSnapshot) -> Vec<Range<Point>> {
    let inlays = folds.inlay_snapshot();
    let mut ranges: Vec<Range<Point>> = folds
        .folds_in_range(InlayPoint::default()..folds.to_inlay_point(folds.max_point()))
        .into_iter()
        .map(|fold| {
            inlays.to_buffer_point(fold.range.start)..inlays.to_buffer_point(fold.range.end)
        })
        .collect();
    ranges.sort_unstable_by_key(|range| (range.start, range.end));
    ranges
}

/// Whether `ranges`, sorted by start and then end, holds `range`.
fn holds(ranges: &[Range<Point>], range: &Range<Point>) -> bool {
    ranges
        .binary_search_by_key(&(range.start, range.end), |probe| (probe.start, probe.end))
        .is_ok()
}

/// The rows an excerpt shows, as runs in buffer order.
///
/// Each hunk's rows widen by [`CONTEXT_ROWS`] on each side, stay inside the
/// buffer's `line_count` rows, and merge with a run they touch. A deletion's
/// rows are empty, so it keeps the row it sits on.
fn kept_runs(map: &DiffMap, line_count: u32) -> Vec<Range<u32>> {
    let mut runs: Vec<Range<u32>> = Vec::new();
    for hunk in map.hunks() {
        let rows = match hunk.buffer_line_range.is_empty() {
            true => hunk.buffer_start_line..hunk.buffer_start_line + 1,
            false => hunk.buffer_line_range.clone(),
        };
        let run =
            rows.start.saturating_sub(CONTEXT_ROWS)..(rows.end + CONTEXT_ROWS).min(line_count);
        if run.is_empty() {
            continue;
        }
        match runs.last_mut() {
            Some(last) if run.start <= last.end => last.end = last.end.max(run.end),
            _ => runs.push(run),
        }
    }
    runs
}

/// The rows of `0..line_count` that no run of `kept` holds, as runs.
fn gaps(kept: &[Range<u32>], line_count: u32) -> Vec<Range<u32>> {
    let mut gaps = Vec::new();
    let mut start = 0;
    for run in kept {
        if run.start > start {
            gaps.push(start..run.start);
        }
        start = run.end;
    }
    if start < line_count {
        gaps.push(start..line_count);
    }
    gaps
}

/// The fold that hides the rows of `gap`.
///
/// A gap below a shown row folds from the end of that row, so the row ends in
/// the fold's placeholder and the row below the gap starts the next display
/// row. A gap at the top folds from the buffer's start, so its placeholder
/// takes a display row of its own.
fn fold_range(rope: &Rope, gap: Range<u32>) -> Range<Point> {
    let end = Point::new(gap.end - 1, rope.line_len(gap.end - 1));
    match gap.start {
        0 => Point::new(0, 0)..end,
        start => Point::new(start - 1, rope.line_len(start - 1))..end,
    }
}

/// The version of buffer `id`'s diff map, or `None` while it has none.
fn map_version(buffers: &BufferRegistry, id: BufferId) -> Option<usize> {
    let buffer = buffers.get(id)?;
    let buffer = buffer.read().expect("buffer poisoned");
    buffer.diff_map.as_ref().map(DiffMap::version)
}

#[cfg(test)]
mod tests {
    use super::{
        excerpt_rows, fold_range, gaps, kept_runs, landing_row, pump_diff_changes_stack,
        DiffChangesState, Landing, StackStamp,
    };
    use crate::{
        action_handlers::dispatch,
        buffer::BufferId,
        diff_map::DiffMap,
        display_map::{DisplayPoint, InlayPoint},
        host::FakeRepoBuilder,
        test_harness::TestHarness,
        workspace::{
            diff::{DiffBase, WorktreeBase},
            Workspace,
        },
    };
    use std::{
        ops::Range,
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat_action::CloseDiffChanges;
    use stoat_language::structural_diff;
    use stoat_text::{Point, Rope};

    /// The map a structural diff of `base` against `text` gives.
    fn map_of(base: &str, text: &str) -> DiffMap {
        DiffMap::from_structural_changes(
            structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
        )
    }

    /// Lines `1` to `n`, one per line.
    fn numbered(n: u32) -> String {
        (1..=n).map(|line| format!("{line}\n")).collect()
    }

    /// A hunk keeps three rows each side, a run held at the buffer's ends, and
    /// two runs that touch merge into one. A deletion keeps the row it sits on.
    #[test]
    fn kept_runs_widen_each_hunk_and_merge_where_they_touch() {
        let base = numbered(30);
        let text = base
            .replace("\n2\n", "\nTWO\n")
            .replace("\n9\n", "\nNINE\n")
            .replace("\n20\n", "\n")
            .replace("\n30\n", "\nTHIRTY\n");
        // TWO sits on row 1, NINE on row 8, the deletion of 20 before row 19,
        // and THIRTY on row 28. The buffer has 30 rows, the empty last one
        // included.
        assert_eq!(
            kept_runs(&map_of(&base, &text), 30),
            [0..12, 16..23, 25..30],
        );
    }

    /// The gaps are every row no run holds, at the top, between runs, and at
    /// the bottom.
    #[test]
    fn gaps_are_the_rows_between_and_around_the_runs() {
        let every_row = |line_count: u32| 0..line_count;
        assert_eq!(
            [
                gaps(&[2..5, 8..9], 12),
                gaps(&[every_row(12)], 12),
                gaps(&[], 3)
            ],
            [vec![0..2, 5..8, 9..12], vec![], vec![every_row(3)]],
        );
    }

    /// A gap below a shown row folds from that row's end, and a gap at the top
    /// folds from the buffer's start.
    #[test]
    fn a_fold_starts_at_the_end_of_the_row_above_its_gap() {
        let rope = Rope::from("one\ntwo\nthree\nfour\n");
        assert_eq!(
            [fold_range(&rope, 1..3), fold_range(&rope, 0..2)],
            [
                Point::new(0, 3)..Point::new(2, 5),
                Point::new(0, 0)..Point::new(1, 3)
            ],
        );
    }

    /// A shown row stays, a hidden row moves to the next run below, and a
    /// row below every run moves to the first run.
    #[test]
    fn a_landing_on_a_hidden_row_moves_to_a_shown_one() {
        let kept = [2..5, 8..9];
        assert_eq!(
            [3, 0, 6, 10].map(|row| landing_row(&kept, row)),
            [3, 2, 8, 2]
        );
    }

    /// A landing on a row its excerpt shows holds its screen row for the next
    /// paint. A landing that a fold moves to another row shows another line,
    /// so it holds none.
    #[test]
    fn only_a_landing_on_a_shown_row_holds_its_screen_row() {
        let mut h = TestHarness::with_size(100, 24);
        let base = numbered(20);
        h.stage_review_scenario(
            "/repo",
            &[("a.txt", &base, &base.replace("\n10\n", "\nTEN\n"))],
        );
        stacked(&mut h);
        let land_on = |row| {
            let state = h
                .stoat
                .active_workspace_mut()
                .focused_diff_changes_mut()
                .expect("a changes view");
            state.pending_landing = Some(Landing {
                buffer_id: state.excerpts[0].buffer_id,
                rel_path: "a.txt".to_string(),
                row,
                column: 0,
                screen_row: Some(3),
            });
            pump_diff_changes_stack(&mut h.stoat);
            h.stoat
                .active_workspace()
                .focused_diff_changes()
                .and_then(|state| state.held_screen_row)
        };
        // TEN sits on row 9, inside the shown run of rows 6 to 12, and row 2
        // lies in the fold above it.
        assert_eq!([9, 2].map(land_on), [Some(3), None]);
    }

    /// The scroll moves the least distance that shows the row, and stays for a
    /// row already in view or for a viewport of no rows.
    #[test]
    fn ensure_row_visible_scrolls_the_least_distance() {
        let scrolled = |row, rows| {
            let mut state = DiffChangesState {
                scroll: 5,
                ..DiffChangesState::default()
            };
            state.ensure_row_visible(row, rows);
            state.scroll
        };
        assert_eq!(
            [
                scrolled(2, 4),
                scrolled(9, 4),
                scrolled(8, 4),
                scrolled(9, 0)
            ],
            [2, 6, 5, 5],
        );
    }

    /// The repository at /repo, with the harness's fake files under it.
    fn repo(h: &mut TestHarness) -> FakeRepoBuilder<'_> {
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git()
            .add_repo(PathBuf::from("/repo"))
            .with_fs(h.fake_fs())
    }

    /// The focused changes view's excerpts as each file's path and whether its
    /// folds are cut.
    fn excerpts(h: &TestHarness) -> Vec<(String, bool)> {
        h.stoat
            .active_workspace()
            .focused_diff_changes()
            .expect("a focused changes view")
            .excerpts
            .iter()
            .map(|excerpt| (excerpt.rel_path.clone(), excerpt.folded_version.is_some()))
            .collect()
    }

    fn listed(paths: &[&str]) -> Vec<(String, bool)> {
        paths.iter().map(|path| (path.to_string(), true)).collect()
    }

    /// A changes view over the harness's focused view, settled.
    fn stacked(h: &mut TestHarness) {
        h.cover_with_diff_changes(false);
        h.settle();
    }

    /// Each changed file gets an excerpt, folded, in path order, and an
    /// untracked file beside them gets none.
    #[test]
    fn changed_files_build_folded_excerpts_in_path_order() {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario(
            "/repo",
            &[
                ("c.txt", "c\n", "C\n"),
                ("a.txt", "a\n", "A\n"),
                ("b.txt", "b\n", "B\n"),
            ],
        );
        repo(&mut h).untracked("z.txt");
        h.fake_fs()
            .insert_file(Path::new("/repo/z.txt"), b"scratch\n");

        stacked(&mut h);
        assert_eq!(excerpts(&h), listed(&["a.txt", "b.txt", "c.txt"]));
    }

    /// An excerpt shows its hunk with three rows of context each side, and the
    /// rows beyond them fold away.
    #[test]
    fn an_excerpt_folds_every_row_beyond_the_context_of_its_hunks() {
        let mut h = TestHarness::with_size(100, 24);
        let base = numbered(20);
        h.stage_review_scenario(
            "/repo",
            &[("a.txt", &base, &base.replace("\n10\n", "\nTEN\n"))],
        );
        stacked(&mut h);

        let ws = h.stoat.active_workspace_mut();
        let excerpt = &ws.focused_diff_changes().expect("a changes view").excerpts[0];
        let editor = ws.editors.get_mut(excerpt.editor_id).expect("an editor");
        let snapshot = editor.display_map.snapshot();
        let shown: Vec<u32> = (0..snapshot.line_count())
            .filter_map(|row| snapshot.display_to_buffer(DisplayPoint::new(row, 0)))
            .map(|point| point.row)
            .collect();
        // The placeholder row holds the folded top, then rows 6 to 12 show the
        // hunk at row 9 with its context, and the rows below fold into row 12.
        // The base line the hunk replaced takes a row with no buffer row.
        assert_eq!(shown, [0, 6, 7, 8, 9, 10, 11, 12]);
    }

    /// An unsaved edit to a clean open file puts the file in the list, which
    /// reads the open buffer's diff map.
    #[test]
    fn an_unsaved_edit_to_a_clean_file_gets_an_excerpt() {
        let mut h = TestHarness::with_size(100, 24);
        h.stoat.set_diff_warm_auto(true);
        repo(&mut h)
            .head_file("d.txt", "same\n")
            .head_file("f.txt", "kept\n");
        h.fake_fs().insert_file(Path::new("/repo/d.txt"), b"same\n");
        h.fake_fs().insert_file(Path::new("/repo/f.txt"), b"kept\n");
        h.open_file(Path::new("/repo/f.txt"));
        h.open_file(Path::new("/repo/d.txt"));
        h.type_keys("i n e w Escape");
        h.settle_diff_jobs();

        stacked(&mut h);
        assert_eq!(excerpts(&h), listed(&["d.txt"]));
    }

    /// A stack over the changed a.txt with the clean d.txt open under it, and
    /// d.txt's buffer.
    fn stack_beside_clean_file(h: &mut TestHarness) -> BufferId {
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        repo(h).head_file("d.txt", "same\n");
        h.fake_fs().insert_file(Path::new("/repo/d.txt"), b"same\n");
        h.open_file(Path::new("/repo/d.txt"));
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
        stacked(h);
        buffer_id
    }

    /// Write `text` at the start of buffer `id`, unsaved.
    fn prepend(h: &TestHarness, id: BufferId, text: &str) {
        h.stoat
            .active_workspace()
            .buffers
            .get(id)
            .expect("buffer")
            .write()
            .expect("buffer poisoned")
            .edit(0..0, text);
    }

    /// An excerpt's rows follow its editor's display rows, after a refold and
    /// after an edit that adds a row before any diff map lands.
    #[test]
    fn excerpt_rows_follow_a_refold_and_an_edit() {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
        stacked(&mut h);
        let rows = |h: &mut TestHarness| {
            let ws = h.stoat.active_workspace_mut();
            let id = ws.focused_diff_changes_id().expect("a changes view");
            let Workspace {
                diff_changes_views,
                editors,
                ..
            } = ws;
            let excerpt = &diff_changes_views[id].excerpts[0];
            let shown = excerpt_rows(excerpt, editors);
            let display = editors[excerpt.editor_id].display_map.snapshot();
            (shown, display.line_count() as usize)
        };

        let folded = rows(&mut h);
        prepend(&h, buffer_id, "new\n");
        let edited = rows(&mut h);
        let count = folded.1;
        assert_eq!([folded, edited], [(count, count), (count + 1, count + 1)]);
    }

    /// An unsaved edit to a clean open file joins a stack that already
    /// exists, once the file's diff map lands.
    #[test]
    fn a_landing_outside_the_stack_relists_it() {
        let mut h = TestHarness::with_size(100, 24);
        let clean = stack_beside_clean_file(&mut h);
        let before = excerpts(&h);

        prepend(&h, clean, "new ");
        h.settle_diff_map(clean);
        assert_eq!(
            (before, excerpts(&h)),
            (listed(&["a.txt"]), listed(&["a.txt", "d.txt"])),
        );
    }

    /// A relist reads the root the workspace resolved, so a moved stamp lists
    /// the files again with no repository opened.
    #[test]
    fn a_relist_opens_no_repository() {
        let mut h = TestHarness::with_size(100, 24);
        let clean = stack_beside_clean_file(&mut h);
        let opened = h.fake_git().discover_calls();

        prepend(&h, clean, "new ");
        h.stoat
            .active_workspace_mut()
            .install_test_diff_map(clean, map_of("same\n", "new same\n"));
        h.settle();
        assert_eq!(
            (excerpts(&h), h.fake_git().discover_calls() - opened),
            (listed(&["a.txt", "d.txt"]), 0),
        );
    }

    /// A base switch moves every excerpt's diff map, so each excerpt cuts its
    /// folds again.
    #[test]
    fn a_base_switch_refolds_the_excerpts() {
        let mut h = TestHarness::with_size(100, 24);
        h.stoat.set_diff_warm_auto(true);
        h.stage_index_scenario("/repo", &[("e.txt", "head\n", "index\n", "work\n")]);
        // Set straight on the workspace, since the base actions also turn the
        // diff view on in the focused pane.
        h.stoat
            .active_workspace_mut()
            .set_worktree_base(WorktreeBase::Index);
        stacked(&mut h);
        let folded = |h: &TestHarness| {
            h.stoat
                .active_workspace()
                .focused_diff_changes()
                .and_then(|state| state.excerpts.first())
                .and_then(|excerpt| excerpt.folded_version)
        };
        let index = folded(&h);

        h.stoat
            .active_workspace_mut()
            .set_worktree_base(WorktreeBase::Head);
        h.settle_diff_jobs();
        let head = folded(&h);
        assert_eq!(
            (index.is_some(), head.is_some(), index != head),
            (true, true, true)
        );
    }

    /// One excerpt's folds: the diff map version they were cut from, the fold
    /// snapshot's version, and each fold's id with the buffer rows it hides.
    #[derive(Debug, PartialEq)]
    struct Cut {
        map: Option<usize>,
        version: usize,
        folds: Vec<(usize, Range<u32>)>,
    }

    /// A changes view over a file of 60 numbered lines with a hunk on rows 10,
    /// 30, and 50, so its folds hide rows 0..7, 14..27, 34..47, and 54..61.
    /// Diff maps stay warm, as in the app, so an edit refolds the excerpt.
    fn three_hunk_stack() -> TestHarness {
        let mut h = TestHarness::with_size(100, 24);
        h.stoat.set_diff_warm_auto(true);
        let base = numbered(60);
        let text = base
            .replace("\n11\n", "\nELEVEN\n")
            .replace("\n31\n", "\nTHIRTY-ONE\n")
            .replace("\n51\n", "\nFIFTY-ONE\n");
        h.stage_review_scenario("/repo", &[("a.txt", &base, &text)]);
        h.open_file(Path::new("/repo/a.txt"));
        h.cover_with_diff_changes(false);
        h.settle_diff_jobs();
        h
    }

    /// Replace the first `from` in /repo/a.txt with `to`, and let the changes
    /// view refold over the diff map that follows.
    fn edit(h: &mut TestHarness, from: &str, to: &str) {
        let buffer_id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(Path::new("/repo/a.txt"))
            .expect("an open buffer");
        let start = h
            .buffer_text(buffer_id)
            .find(from)
            .expect("the replaced text");
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("a buffer")
            .write()
            .expect("buffer poisoned")
            .edit(start..start + from.len(), to);
        h.settle_diff_jobs();
    }

    /// The folds of the focused changes view's first excerpt.
    fn cut(h: &mut TestHarness) -> Cut {
        let ws = h.stoat.active_workspace_mut();
        let excerpt = &ws.focused_diff_changes().expect("a changes view").excerpts[0];
        let map = excerpt.folded_version;
        let editor = ws.editors.get_mut(excerpt.editor_id).expect("an editor");
        let snapshot = editor.display_map.snapshot();
        let folds = snapshot.fold_snapshot();
        let inlays = folds.inlay_snapshot();
        let hidden = |range: &Range<InlayPoint>| {
            let start = inlays.to_buffer_point(range.start);
            let first = if start == Point::new(0, 0) {
                0
            } else {
                start.row + 1
            };
            first..inlays.to_buffer_point(range.end).row + 1
        };
        Cut {
            map,
            version: folds.version(),
            folds: folds
                .folds_in_range(InlayPoint::default()..folds.to_inlay_point(folds.max_point()))
                .into_iter()
                .map(|fold| (fold.id.0, hidden(&fold.range)))
                .collect(),
        }
    }

    /// The rows each fold of `after` hides, with whether `before` held it.
    fn survivors(before: &Cut, after: &Cut) -> Vec<(bool, Range<u32>)> {
        after
            .folds
            .iter()
            .map(|(id, rows)| {
                (
                    before.folds.iter().any(|(held, _)| held == id),
                    rows.clone(),
                )
            })
            .collect()
    }

    /// A landing whose map shows the same rows leaves every fold, so the
    /// display map does no work for it.
    #[test]
    fn a_landing_that_keeps_the_shown_rows_toggles_no_fold() {
        let mut h = three_hunk_stack();
        let before = cut(&mut h);
        edit(&mut h, "ELEVEN", "ELEVEN!");
        let after = cut(&mut h);
        assert_eq!(
            (after.map != before.map, after.version, after.folds),
            (true, before.version, before.folds),
        );
    }

    /// A hunk that lands in a gap refolds that gap, and the other folds stay.
    #[test]
    fn a_hunk_landing_in_a_gap_refolds_that_gap_only() {
        let mut h = three_hunk_stack();
        let before = cut(&mut h);
        // Row 36 lies three rows below the run around row 30, so the run grows
        // to row 40 and the gap below it shrinks.
        edit(&mut h, "\n37\n", "\nTHIRTY-SEVEN\n");
        assert_eq!(
            survivors(&before, &cut(&mut h)),
            [
                (true, 0..7),
                (true, 14..27),
                (false, 40..47),
                (true, 54..61)
            ],
        );
    }

    /// A run whose only hunk goes leaves one fold over both gaps around it,
    /// and the other folds stay.
    #[test]
    fn a_run_whose_hunk_goes_joins_the_gaps_around_it() {
        let mut h = three_hunk_stack();
        let before = cut(&mut h);
        edit(&mut h, "THIRTY-ONE", "31");
        assert_eq!(
            survivors(&before, &cut(&mut h)),
            [(true, 0..7), (false, 14..47), (true, 54..61)],
        );
    }

    /// A row added inside a hunk moves the folds below it a row down, and every
    /// fold stays.
    #[test]
    fn a_row_added_inside_a_hunk_keeps_every_fold() {
        let mut h = three_hunk_stack();
        let before = cut(&mut h);
        edit(&mut h, "ELEVEN", "ELEVEN\nMORE");
        assert_eq!(
            survivors(&before, &cut(&mut h)),
            [(true, 0..7), (true, 15..28), (true, 35..48), (true, 55..62)],
        );
    }

    /// A stage and a `.git` event move the base where no diff map moved yet,
    /// so each moves the stamp, and the list follows.
    #[test]
    fn a_stage_and_a_git_event_each_move_the_stamp() {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
        let stamp = |h: &TestHarness| StackStamp::read(h.stoat.active_workspace());

        let first = stamp(&h);
        h.stoat
            .active_workspace_mut()
            .invalidate_diff(buffer_id, Path::new("/repo/a.txt"));
        let staged = stamp(&h);
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        assert_eq!((first != staged, staged != stamp(&h)), (true, true));
    }

    /// An open and a close move the stamp's path part, and a diff landing its
    /// map part, since the list reads the open buffers' diff maps.
    #[test]
    fn an_open_a_landing_and_a_close_move_their_stamp_parts() {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        let parts = |h: &TestHarness| {
            let stamp = StackStamp::read(h.stoat.active_workspace());
            (stamp.maps, stamp.paths)
        };
        let mut seen = vec![parts(&h)];

        h.open_file(Path::new("/repo/a.txt"));
        seen.push(parts(&h));
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
        h.settle_diff_map(buffer_id);
        seen.push(parts(&h));
        h.stoat.active_workspace_mut().buffers.remove(buffer_id);
        seen.push(parts(&h));
        let moved: Vec<(bool, bool)> = seen
            .windows(2)
            .map(|pair| (pair[0].0 != pair[1].0, pair[0].1 != pair[1].1))
            .collect();
        assert_eq!(moved, [(false, true), (true, false), (false, true)]);
    }

    /// Under a revision base the stack lists what changed since the revision,
    /// a file committed since and clean in the tree included.
    #[test]
    fn a_revision_base_lists_what_changed_since_the_revision() {
        let mut h = TestHarness::with_size(100, 24);
        repo(&mut h)
            .head_file("committed.txt", "after\n")
            .commit("base0", &[("committed.txt", "before\n")]);
        h.fake_fs()
            .insert_file(Path::new("/repo/committed.txt"), b"after\n");
        h.stoat
            .active_workspace_mut()
            .set_diff_base(Some(DiffBase::named("base0", "base0".to_string())));

        stacked(&mut h);
        assert_eq!(excerpts(&h), listed(&["committed.txt"]));
    }

    /// A close drops the buffers only the stack opened, and keeps the one a
    /// pane opened before.
    #[test]
    fn a_close_drops_the_buffers_only_the_stack_opened() {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        stacked(&mut h);
        let open = |h: &TestHarness, path: &str| {
            h.stoat
                .active_workspace()
                .buffers
                .id_for_path(Path::new(path))
                .is_some()
        };
        let before = (open(&h, "/repo/a.txt"), open(&h, "/repo/b.txt"));

        dispatch(&mut h.stoat, &CloseDiffChanges);
        assert_eq!(
            (before, open(&h, "/repo/a.txt"), open(&h, "/repo/b.txt")),
            ((true, true), true, false),
        );
    }
}
