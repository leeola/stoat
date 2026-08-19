use crate::{
    buffer_registry::BufferRegistry,
    input_view::InputView,
    jumplist::{JumpEntry, JumpList},
    picker::{Preview, PreviewSource, TargetPicker},
};
use std::path::{Path, PathBuf};
use stoat_text::{Anchor, BufferId, Selection};

/// Modal listing every entry in the focused pane's [`JumpList`].
///
/// Each row is a pre-formatted `(filename, line, column, snippet)` resolved
/// from the entry's own buffer, so a cross-buffer jumplist renders every file
/// it spans. The picker owns its rows rather than borrowing back into the
/// workspace, so render runs without re-entering buffer locks.
///
/// Navigation, filtering, and selection route through the
/// `modal == jumplist && mode == insert` keymap block. A query narrows the
/// list against [`jumplist_haystack`], and [`Self::selected_index`] reports
/// which jumplist entry the surviving row names.
pub struct JumplistPicker {
    /// The filter, selection, and preview every target list shares.
    pub(crate) picker: TargetPicker<JumplistEntry>,
    cursor_idx: usize,
}

pub struct JumplistEntry {
    pub filename: String,
    pub line: u32,
    pub column: u32,
    pub snippet: String,
    /// The buffer the jump points into. The preview reads it directly, so an
    /// entry into edited text shows what the reader has on screen rather than
    /// what is on disk.
    pub buffer_id: BufferId,
    /// The buffer's file, when it has one. Only a fallback for the preview:
    /// a scratch buffer has none, and a closed one is no longer readable by id.
    pub path: Option<PathBuf>,
}

const SNIPPET_MAX_CHARS: usize = 80;

impl JumplistPicker {
    /// Wrap `entries` with the prompt and preview, opening on `cursor_idx`.
    ///
    /// The selection starts on the entry the walk cursor would jump from, so
    /// Enter alone repeats the last jump.
    pub(crate) fn from_entries(
        entries: Vec<JumplistEntry>,
        cursor_idx: usize,
        input: InputView,
        preview: Preview,
    ) -> Self {
        let haystacks = entries.iter().map(jumplist_haystack).collect();
        let selected = cursor_idx.min(entries.len().saturating_sub(1));
        let mut picker = TargetPicker::new(entries, haystacks, input, preview);
        picker.move_selection(selected as i32);
        Self { picker, cursor_idx }
    }

    /// One row per jump in `jumplist`, each resolved against its own buffer.
    ///
    /// Each entry's newest selection head becomes a `(line, column)` point, a
    /// one-line snippet, and a file name. An entry whose buffer is gone lists
    /// as `[scratch]` with an empty location. An empty jumplist yields no
    /// rows, which callers treat as a reason not to open the modal at all.
    pub fn entries_from(jumplist: &JumpList, buffers: &BufferRegistry) -> Vec<JumplistEntry> {
        jumplist
            .entries()
            .iter()
            .map(|entry| entry_row(entry, buffers))
            .collect()
    }

    pub fn entries(&self) -> &[JumplistEntry] {
        self.picker.entries()
    }

    pub(crate) fn filtered(&self) -> &[usize] {
        self.picker.filtered()
    }

    /// Cursor into the filtered rows, not into the entries. A caller wanting
    /// the jumplist entry takes [`Self::selected_index`].
    pub fn selected(&self) -> usize {
        self.picker.selected()
    }

    /// Index into the [`JumpList`] of the row under the selection, or [`None`]
    /// when the filter matched nothing.
    ///
    /// The rows are built in jumplist order, so this indexes the jumplist
    /// itself and is what the jump and the cursor update both read.
    pub(crate) fn selected_index(&self) -> Option<usize> {
        self.picker.filtered().get(self.picker.selected()).copied()
    }

    /// Index of the entry the [`JumpList`] cursor would walk from on the next
    /// [`JumpList::backward`]. Equal to `entries.len()` when the cursor is past
    /// the end of the stack (the default after a fresh record).
    pub fn cursor_idx(&self) -> usize {
        self.cursor_idx
    }

    pub(crate) fn page(&mut self, dir: i32) {
        self.picker.page(dir);
    }

    pub(crate) fn dispose(self, ws: &mut crate::workspace::Workspace) {
        self.picker.dispose(ws);
    }
}

/// What a query matches a jump against. The haystack names where it goes and
/// what is there, which is what the row shows.
fn jumplist_haystack(entry: &JumplistEntry) -> String {
    format!("{}:{} {}", entry.filename, entry.line, entry.snippet)
}

/// Characters of [`jumplist_haystack`] that come before the snippet.
///
/// The renderer highlights only the matched characters that fall in the
/// snippet column, so it subtracts this from every match offset.
pub(crate) fn haystack_prefix_len(entry: &JumplistEntry) -> u32 {
    let haystack = jumplist_haystack(entry);
    (haystack.chars().count() - entry.snippet.chars().count()) as u32
}

/// Where a jump's preview reads from and which 0-based line it centres on.
///
/// The buffer wins over the file, so a jump into edited text previews what the
/// reader has on screen. An entry whose buffer closed falls back on its path,
/// and a scratch entry has neither.
pub(crate) fn jumplist_target(
    ws: &crate::workspace::Workspace,
    entry: &JumplistEntry,
) -> Option<(PreviewSource, u32)> {
    let source = match ws.buffers.get(entry.buffer_id).is_some() {
        true => PreviewSource::Buffer(entry.buffer_id),
        false => PreviewSource::File(entry.path.clone()?),
    };
    Some((source, entry.line.saturating_sub(1)))
}

/// Format one jump entry into a display row, resolving its position against its
/// own buffer. A closed buffer yields `[scratch]` with an empty location.
fn entry_row(entry: &JumpEntry, buffers: &BufferRegistry) -> JumplistEntry {
    let path = buffers.path_for(entry.buffer_id).map(Path::to_path_buf);
    let filename = path
        .as_deref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "[scratch]".to_string());

    let location = resolve_location(entry, buffers);
    let (line, column, snippet) = location.unwrap_or((0, 0, String::new()));
    JumplistEntry {
        filename,
        line,
        column,
        snippet,
        buffer_id: entry.buffer_id,
        path,
    }
}

/// The `(line, column, snippet)` of an entry's newest block-cursor cell, or
/// `None` when the buffer is gone or carries no selection.
fn resolve_location(entry: &JumpEntry, buffers: &BufferRegistry) -> Option<(u32, u32, String)> {
    let selection = newest_selection(entry)?;
    let buffer = buffers.get(entry.buffer_id)?;
    let guard = buffer.read().ok()?;
    let rope = guard.rope();
    let tail = guard.resolve_anchor(&selection.tail()).min(rope.len());
    let head = guard.resolve_anchor(&selection.head()).min(rope.len());
    let offset = stoat_text::cursor_offset(rope, tail, head);
    let point = rope.offset_to_point(offset);
    let raw = rope.line_at_row(point.row);
    let snippet: String = raw.trim_start().chars().take(SNIPPET_MAX_CHARS).collect();
    Some((point.row + 1, point.column + 1, snippet))
}

fn newest_selection(entry: &JumpEntry) -> Option<&Selection<Anchor>> {
    entry.selections.iter().max_by_key(|selection| selection.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer_registry::BufferRegistry;
    use std::path::Path;
    use stoat_text::{Bias, BufferId, Selection, SelectionGoal};

    fn jump_at(buffers: &BufferRegistry, buffer_id: BufferId, offset: usize) -> JumpEntry {
        let buffer = buffers.get(buffer_id).expect("buffer open");
        let guard = buffer.read().expect("buffer readable");
        let anchor = guard.anchor_at(offset, Bias::Right);
        JumpEntry {
            buffer_id,
            selections: vec![Selection {
                id: 0,
                start: anchor,
                end: anchor,
                reversed: false,
                goal: SelectionGoal::None,
            }],
        }
    }

    fn jumplist_over(buffers: &BufferRegistry, buffer_id: BufferId, offsets: &[usize]) -> JumpList {
        let mut jumplist = JumpList::default();
        for &offset in offsets {
            jumplist.push(jump_at(buffers, buffer_id, offset), buffers);
        }
        jumplist
    }

    #[test]
    fn entries_from_lists_every_jump_with_filename_and_line_col() {
        let mut buffers = BufferRegistry::new();
        let (id, _) = buffers.open(Path::new("/dir/file.rs"), "alpha\nbeta\ngamma\n");
        let jl = jumplist_over(&buffers, id, &[0, 6, 11]);
        let entries = JumplistPicker::entries_from(&jl, &buffers);
        assert_eq!(
            entries
                .iter()
                .map(|e| (
                    e.filename.as_str(),
                    e.line,
                    e.column,
                    e.snippet.as_str(),
                    e.buffer_id
                ))
                .collect::<Vec<_>>(),
            [
                ("file.rs", 1, 1, "alpha", id),
                ("file.rs", 2, 1, "beta", id),
                ("file.rs", 3, 1, "gamma", id),
            ],
            "each jump names its file, position, line text, and source buffer"
        );
    }

    #[test]
    fn snippet_strips_leading_whitespace() {
        let mut buffers = BufferRegistry::new();
        let (id, _) = buffers.open(Path::new("/a.rs"), "    indented\nflat\n");
        let jl = jumplist_over(&buffers, id, &[0]);
        let entries = JumplistPicker::entries_from(&jl, &buffers);
        assert_eq!(entries[0].snippet, "indented");
    }

    #[test]
    fn entries_span_multiple_buffers() {
        let mut buffers = BufferRegistry::new();
        let (a, _) = buffers.open(Path::new("/a.rs"), "aaa\n");
        let (b, _) = buffers.open(Path::new("/b.rs"), "bbb\n");
        let mut jl = JumpList::default();
        jl.push(jump_at(&buffers, a, 0), &buffers);
        jl.push(jump_at(&buffers, b, 0), &buffers);
        let entries = JumplistPicker::entries_from(&jl, &buffers);
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.filename.as_str(), e.buffer_id))
                .collect::<Vec<_>>(),
            [("a.rs", a), ("b.rs", b)],
            "each row keeps the buffer its jump points into"
        );
    }

    /// The renderer subtracts this from a match offset to find the column in
    /// the snippet, so it has to count exactly what the haystack puts ahead of
    /// the snippet and no more.
    #[test]
    fn the_haystack_prefix_covers_everything_before_the_snippet() {
        let mut buffers = BufferRegistry::new();
        let (id, _) = buffers.open(Path::new("/dir/file.rs"), "alpha\nbeta\n");
        let jl = jumplist_over(&buffers, id, &[6]);
        let entries = JumplistPicker::entries_from(&jl, &buffers);
        assert_eq!(
            haystack_prefix_len(&entries[0]),
            "file.rs:2 ".len() as u32,
            "the file and position precede the snippet"
        );
    }

    /// A jump previews the buffer it points into rather than the file behind
    /// it, so an edited jump target shows what the reader has on screen.
    #[test]
    fn a_target_reads_the_live_buffer_over_the_file() {
        let mut h = crate::Stoat::test();
        h.seed_focused_buffer("alpha\nbeta\ngamma\n");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);

        let ws = h.stoat.active_workspace();
        let jumplist = ws.panes.pane(ws.panes.focus()).jumplist.clone();
        let entries = JumplistPicker::entries_from(&jumplist, &ws.buffers);
        let entry = &entries[0];
        assert_eq!(
            jumplist_target(ws, entry),
            Some((PreviewSource::Buffer(entry.buffer_id), entry.line - 1)),
            "the open buffer previews at the jump's 0-based line"
        );
    }

    /// Drives the paging bindings through the real keymap, which is what proves
    /// the def, kind, registration, dispatch arm, and binding all reach each
    /// other. The per-picker paging tests exercise the arithmetic. Only a
    /// keypress can show the wiring.
    #[test]
    fn ctrl_f_and_ctrl_b_page_the_jumplist_selection() {
        let mut h = crate::Stoat::test();
        let lines: String = (0..20)
            .map(|i| format!("line {i} of the buffer\n"))
            .collect();
        h.seed_focused_buffer(&lines);
        for _ in 0..20 {
            crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
            crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveDown);
        }
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenJumplistPicker);
        let _ = h.snapshot();

        let selected = |h: &crate::test_harness::TestHarness| {
            h.stoat
                .jumplist_picker
                .as_ref()
                .expect("modal open")
                .selected()
        };
        let start = selected(&h);
        let half = h
            .stoat
            .jumplist_picker
            .as_ref()
            .expect("modal open")
            .picker
            .viewport_rows
            .expect("the render stamped the viewport")
            .div_ceil(2)
            .max(1);
        assert!(
            start >= half,
            "the picker opens at the walk cursor with room to page up: {start} < {half}"
        );

        h.type_keys("ctrl-b");
        assert_eq!(
            selected(&h),
            start - half,
            "Ctrl-B pages up by half the rendered list height"
        );

        h.type_keys("ctrl-f");
        assert_eq!(selected(&h), start, "Ctrl-F pages back down");
    }

    #[test]
    fn snapshot_jumplist_picker_listing() {
        let mut h = crate::Stoat::test();
        h.seed_focused_buffer("alpha first line\n    indented mid\nlast line is here\n");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenJumplistPicker);
        h.assert_snapshot("jumplist_picker_listing");
    }
}
