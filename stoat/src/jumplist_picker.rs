use crate::{
    buffer_registry::BufferRegistry,
    jumplist::{JumpEntry, JumpList},
};
use stoat_text::Anchor;

/// Modal listing every entry in the focused pane's [`JumpList`].
///
/// Each row is a pre-formatted `(filename, line, column, snippet)` resolved
/// from the entry's own buffer, so a cross-buffer jumplist renders every file
/// it spans. The picker owns its rows rather than borrowing back into the
/// workspace, so render runs without re-entering buffer locks.
///
/// Navigation and selection route through the `modal == jumplist` keymap
/// block. [`Self::select_next`] and [`Self::select_prev`] move the highlight,
/// and [`Self::selected`] reports the row to jump to.
pub struct JumplistPicker {
    entries: Vec<JumplistEntry>,
    selected: usize,
    cursor_idx: usize,
}

pub struct JumplistEntry {
    pub filename: String,
    pub line: u32,
    pub column: u32,
    pub snippet: String,
}

const SNIPPET_MAX_CHARS: usize = 80;

impl JumplistPicker {
    /// Build a picker from a pane's [`JumpList`] and the workspace
    /// [`BufferRegistry`]. Each entry's newest selection head is resolved
    /// against its own buffer into a `(line, column)` point, a one-line
    /// snippet, and a file name. Entries whose buffer is gone list as
    /// `[scratch]` with an empty location. Empty input produces an empty
    /// picker, which callers should treat as a no-op rather than open the modal.
    pub fn new(jumplist: &JumpList, buffers: &BufferRegistry) -> Self {
        let entries: Vec<JumplistEntry> = jumplist
            .entries()
            .iter()
            .map(|entry| entry_row(entry, buffers))
            .collect();
        let cursor_idx = jumplist.cursor();
        let selected = cursor_idx.min(entries.len().saturating_sub(1));
        Self {
            entries,
            selected,
            cursor_idx,
        }
    }

    pub fn entries(&self) -> &[JumplistEntry] {
        &self.entries
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Index of the entry the [`JumpList`] cursor would walk from on the next
    /// [`JumpList::backward`]. Equal to `entries.len()` when the cursor is past
    /// the end of the stack (the default after a fresh record).
    pub fn cursor_idx(&self) -> usize {
        self.cursor_idx
    }

    pub fn select_next(&mut self) {
        self.move_selection(1);
    }

    pub fn select_prev(&mut self) {
        self.move_selection(-1);
    }

    pub fn hint_bindings(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Enter", "jump".to_string()),
            ("Esc", "cancel".to_string()),
            ("Ctrl-N", "next".to_string()),
            ("Ctrl-P", "prev".to_string()),
        ]
    }

    fn move_selection(&mut self, delta: i32) {
        if self.entries.is_empty() {
            self.selected = 0;
            return;
        }
        let max = (self.entries.len() - 1) as i32;
        self.selected = (self.selected as i32 + delta).clamp(0, max) as usize;
    }
}

/// Format one jump entry into a display row, resolving its position against its
/// own buffer. A closed buffer yields `[scratch]` with an empty location.
fn entry_row(entry: &JumpEntry, buffers: &BufferRegistry) -> JumplistEntry {
    let filename = buffers
        .path_for(entry.buffer_id)
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
    }
}

/// The `(line, column, snippet)` of an entry's newest selection head, or `None`
/// when the buffer is gone or carries no selection.
fn resolve_location(entry: &JumpEntry, buffers: &BufferRegistry) -> Option<(u32, u32, String)> {
    let head = newest_head(entry)?;
    let buffer = buffers.get(entry.buffer_id)?;
    let guard = buffer.read().ok()?;
    let rope = guard.rope();
    let offset = guard.resolve_anchor(&head).min(rope.len());
    let point = rope.offset_to_point(offset);
    let raw = rope.line_at_row(point.row);
    let snippet: String = raw.trim_start().chars().take(SNIPPET_MAX_CHARS).collect();
    Some((point.row + 1, point.column + 1, snippet))
}

fn newest_head(entry: &JumpEntry) -> Option<Anchor> {
    entry
        .selections
        .iter()
        .max_by_key(|selection| selection.id)
        .map(|selection| selection.head())
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
    fn new_lists_every_entry_with_filename_and_line_col() {
        let mut buffers = BufferRegistry::new();
        let (id, _) = buffers.open(Path::new("/dir/file.rs"), "alpha\nbeta\ngamma\n");
        let jl = jumplist_over(&buffers, id, &[0, 6, 11]);
        let picker = JumplistPicker::new(&jl, &buffers);
        let entries = picker.entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].filename, "file.rs");
        assert_eq!((entries[0].line, entries[0].column), (1, 1));
        assert_eq!(entries[0].snippet, "alpha");
        assert_eq!((entries[1].line, entries[1].column), (2, 1));
        assert_eq!(entries[1].snippet, "beta");
        assert_eq!((entries[2].line, entries[2].column), (3, 1));
        assert_eq!(entries[2].snippet, "gamma");
    }

    #[test]
    fn snippet_strips_leading_whitespace() {
        let mut buffers = BufferRegistry::new();
        let (id, _) = buffers.open(Path::new("/a.rs"), "    indented\nflat\n");
        let jl = jumplist_over(&buffers, id, &[0]);
        let picker = JumplistPicker::new(&jl, &buffers);
        assert_eq!(picker.entries()[0].snippet, "indented");
    }

    #[test]
    fn entries_span_multiple_buffers() {
        let mut buffers = BufferRegistry::new();
        let (a, _) = buffers.open(Path::new("/a.rs"), "aaa\n");
        let (b, _) = buffers.open(Path::new("/b.rs"), "bbb\n");
        let mut jl = JumpList::default();
        jl.push(jump_at(&buffers, a, 0), &buffers);
        jl.push(jump_at(&buffers, b, 0), &buffers);
        let picker = JumplistPicker::new(&jl, &buffers);
        assert_eq!(picker.entries()[0].filename, "a.rs");
        assert_eq!(picker.entries()[1].filename, "b.rs");
    }

    #[test]
    fn select_next_prev_clamp_at_ends() {
        let mut buffers = BufferRegistry::new();
        let (id, _) = buffers.open(Path::new("/a.rs"), "a\nb\nc\n");
        let jl = jumplist_over(&buffers, id, &[0, 2, 4]);
        let mut picker = JumplistPicker::new(&jl, &buffers);
        picker.select_prev();
        picker.select_prev();
        assert_eq!(picker.selected(), 0);
        picker.select_next();
        picker.select_next();
        picker.select_next();
        assert_eq!(picker.selected(), 2);
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
