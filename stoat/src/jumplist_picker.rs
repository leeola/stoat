use crate::{buffer::TextBuffer, jumplist::JumpList};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Modal listing every entry in the focused editor's [`JumpList`].
/// Constructed from a snapshot of the jumplist's offsets paired with
/// (line, column, snippet) pre-formatted for render. The picker does
/// not borrow back into the workspace, so render and key dispatch can
/// run without re-entering buffer locks.
pub struct JumplistPicker {
    entries: Vec<JumplistEntry>,
    selected: usize,
    cursor_idx: usize,
}

pub struct JumplistEntry {
    pub offset: usize,
    pub line: u32,
    pub column: u32,
    pub snippet: String,
}

pub enum PickerOutcome {
    /// Re-render but keep the modal open.
    None,
    /// User cancelled; caller should drop the modal.
    Close,
    /// User selected entry index `usize`; caller should jump and drop
    /// the modal.
    Select(usize),
}

const SNIPPET_MAX_CHARS: usize = 80;

impl JumplistPicker {
    /// Build a picker from the focused editor's [`JumpList`] and its
    /// associated [`TextBuffer`]. Each offset is converted to a
    /// `(line, column)` point and a one-line snippet of the rope at
    /// that line. Stale offsets past the rope end clamp to the end.
    /// Empty input produces an empty picker; callers should treat that
    /// as a no-op rather than open the modal.
    pub fn new(jumplist: &JumpList, buffer: &TextBuffer) -> Self {
        let rope = buffer.rope();
        let rope_len = rope.len();
        let entries: Vec<JumplistEntry> = jumplist
            .entries()
            .iter()
            .map(|&offset| {
                let clipped = offset.min(rope_len);
                let point = rope.offset_to_point(clipped);
                let raw = rope.line_at_row(point.row);
                let trimmed = raw.trim_start();
                let snippet: String = trimmed.chars().take(SNIPPET_MAX_CHARS).collect();
                JumplistEntry {
                    offset: clipped,
                    line: point.row + 1,
                    column: point.column + 1,
                    snippet,
                }
            })
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

    /// Index of the entry the [`JumpList`] cursor would walk from on
    /// the next [`JumpList::backward`]. Equal to `entries.len()` when
    /// the cursor is past the end of the stack (the default after a
    /// fresh `save()`).
    pub fn cursor_idx(&self) -> usize {
        self.cursor_idx
    }

    pub fn hint_bindings(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Enter", "jump".to_string()),
            ("Esc", "cancel".to_string()),
            ("Ctrl-N", "next".to_string()),
            ("Ctrl-P", "prev".to_string()),
        ]
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        match key.code {
            KeyCode::Esc => PickerOutcome::Close,
            KeyCode::Enter => match self.entries.get(self.selected) {
                Some(_) => PickerOutcome::Select(self.selected),
                None => PickerOutcome::Close,
            },
            KeyCode::Up => {
                self.move_selection(-1);
                PickerOutcome::None
            },
            KeyCode::Down => {
                self.move_selection(1);
                PickerOutcome::None
            },
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(-1);
                PickerOutcome::None
            },
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(1);
                PickerOutcome::None
            },
            _ => PickerOutcome::None,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{buffer::BufferId, test_harness::keys};

    fn buf(text: &str) -> TextBuffer {
        TextBuffer::with_text(BufferId::new(1), text)
    }

    fn jumplist(positions: &[usize]) -> JumpList {
        let mut j = JumpList::new();
        for &p in positions {
            j.save(p);
        }
        j
    }

    #[test]
    fn new_lists_every_entry_with_line_col() {
        let buffer = buf("alpha\nbeta\ngamma\n");
        let jl = jumplist(&[0, 6, 11]);
        let picker = JumplistPicker::new(&jl, &buffer);
        let entries = picker.entries();
        assert_eq!(entries.len(), 3);
        assert_eq!((entries[0].line, entries[0].column), (1, 1));
        assert_eq!(entries[0].snippet, "alpha");
        assert_eq!((entries[1].line, entries[1].column), (2, 1));
        assert_eq!(entries[1].snippet, "beta");
        assert_eq!((entries[2].line, entries[2].column), (3, 1));
        assert_eq!(entries[2].snippet, "gamma");
    }

    #[test]
    fn new_clamps_offset_past_rope_end() {
        let buffer = buf("hi\n");
        let jl = jumplist(&[999]);
        let picker = JumplistPicker::new(&jl, &buffer);
        assert_eq!(picker.entries()[0].offset, 3);
    }

    #[test]
    fn snippet_strips_leading_whitespace() {
        let buffer = buf("    indented\nflat\n");
        let jl = jumplist(&[0]);
        let picker = JumplistPicker::new(&jl, &buffer);
        assert_eq!(picker.entries()[0].snippet, "indented");
    }

    #[test]
    fn enter_returns_select() {
        let buffer = buf("a\nb\n");
        let jl = jumplist(&[0, 2]);
        let mut picker = JumplistPicker::new(&jl, &buffer);
        assert!(matches!(
            picker.handle_key(keys::key(KeyCode::Enter)),
            PickerOutcome::Select(_)
        ));
    }

    #[test]
    fn esc_returns_close() {
        let buffer = buf("a\nb\n");
        let jl = jumplist(&[0]);
        let mut picker = JumplistPicker::new(&jl, &buffer);
        assert!(matches!(
            picker.handle_key(keys::key(KeyCode::Esc)),
            PickerOutcome::Close
        ));
    }

    #[test]
    fn down_and_up_clamp_at_ends() {
        let buffer = buf("a\nb\nc\n");
        let jl = jumplist(&[0, 2, 4]);
        let mut picker = JumplistPicker::new(&jl, &buffer);
        picker.handle_key(keys::key(KeyCode::Up));
        picker.handle_key(keys::key(KeyCode::Up));
        assert_eq!(picker.selected(), 0);
        picker.handle_key(keys::key(KeyCode::Down));
        picker.handle_key(keys::key(KeyCode::Down));
        picker.handle_key(keys::key(KeyCode::Down));
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
