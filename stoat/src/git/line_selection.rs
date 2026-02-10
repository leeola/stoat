//! Line-level selection within a diff hunk for partial staging.

use crate::git::diff::{HunkLineOrigin, HunkLines};

/// Per-line selection state within a single diff hunk.
///
/// Allows toggling individual `+`/`-` lines for partial staging or unstaging.
/// Context lines are always included and not selectable.
pub struct LineSelection {
    pub hunk_lines: HunkLines,
    pub selected: Vec<bool>,
    pub cursor_line: usize,
}

impl LineSelection {
    /// Create a new selection with all changeable lines selected.
    pub fn new(hunk_lines: HunkLines) -> Self {
        let selected = hunk_lines
            .lines
            .iter()
            .map(|l| l.origin != HunkLineOrigin::Context)
            .collect();
        let cursor_line = hunk_lines
            .lines
            .iter()
            .position(|l| l.origin != HunkLineOrigin::Context)
            .unwrap_or(0);
        Self {
            hunk_lines,
            selected,
            cursor_line,
        }
    }

    pub fn toggle_line(&mut self) {
        if self.is_changeable(self.cursor_line) {
            self.selected[self.cursor_line] = !self.selected[self.cursor_line];
        }
    }

    pub fn select_all(&mut self) {
        for (i, line) in self.hunk_lines.lines.iter().enumerate() {
            if line.origin != HunkLineOrigin::Context {
                self.selected[i] = true;
            }
        }
    }

    pub fn deselect_all(&mut self) {
        for (i, line) in self.hunk_lines.lines.iter().enumerate() {
            if line.origin != HunkLineOrigin::Context {
                self.selected[i] = false;
            }
        }
    }

    pub fn move_cursor_down(&mut self) {
        for i in (self.cursor_line + 1)..self.hunk_lines.lines.len() {
            if self.is_changeable(i) {
                self.cursor_line = i;
                return;
            }
        }
    }

    pub fn move_cursor_up(&mut self) {
        for i in (0..self.cursor_line).rev() {
            if self.is_changeable(i) {
                self.cursor_line = i;
                return;
            }
        }
    }

    pub fn has_selection(&self) -> bool {
        self.selected.iter().any(|&s| s)
    }

    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|&&s| s).count()
    }

    pub fn total_changeable_count(&self) -> usize {
        self.hunk_lines
            .lines
            .iter()
            .filter(|l| l.origin != HunkLineOrigin::Context)
            .count()
    }

    fn is_changeable(&self, idx: usize) -> bool {
        self.hunk_lines
            .lines
            .get(idx)
            .map(|l| l.origin != HunkLineOrigin::Context)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::{HunkLine, HunkLines};

    fn make_hunk_lines(origins: &[HunkLineOrigin]) -> HunkLines {
        HunkLines {
            old_start: 1,
            old_lines: origins
                .iter()
                .filter(|o| matches!(o, HunkLineOrigin::Deletion | HunkLineOrigin::Context))
                .count() as u32,
            new_start: 1,
            new_lines: origins
                .iter()
                .filter(|o| matches!(o, HunkLineOrigin::Addition | HunkLineOrigin::Context))
                .count() as u32,
            lines: origins
                .iter()
                .enumerate()
                .map(|(i, &origin)| HunkLine {
                    origin,
                    content: format!("line {i}\n"),
                    old_lineno: None,
                    new_lineno: None,
                })
                .collect(),
        }
    }

    #[test]
    fn all_changeable_lines_selected_by_default() {
        let sel = LineSelection::new(make_hunk_lines(&[
            HunkLineOrigin::Deletion,
            HunkLineOrigin::Addition,
        ]));
        assert_eq!(sel.selected_count(), 2);
        assert_eq!(sel.total_changeable_count(), 2);
    }

    #[test]
    fn toggle_line() {
        let mut sel = LineSelection::new(make_hunk_lines(&[
            HunkLineOrigin::Deletion,
            HunkLineOrigin::Addition,
        ]));
        sel.toggle_line();
        assert_eq!(sel.selected_count(), 1);
        assert!(!sel.selected[0]);
        assert!(sel.selected[1]);
    }

    #[test]
    fn deselect_all_then_select_all() {
        let mut sel = LineSelection::new(make_hunk_lines(&[
            HunkLineOrigin::Deletion,
            HunkLineOrigin::Addition,
        ]));
        sel.deselect_all();
        assert!(!sel.has_selection());
        sel.select_all();
        assert_eq!(sel.selected_count(), 2);
    }

    #[test]
    fn cursor_skips_context_lines() {
        let mut sel = LineSelection::new(make_hunk_lines(&[
            HunkLineOrigin::Context,
            HunkLineOrigin::Deletion,
            HunkLineOrigin::Context,
            HunkLineOrigin::Addition,
        ]));
        assert_eq!(sel.cursor_line, 1);
        sel.move_cursor_down();
        assert_eq!(sel.cursor_line, 3);
        sel.move_cursor_down();
        assert_eq!(sel.cursor_line, 3, "stays at last changeable line");
        sel.move_cursor_up();
        assert_eq!(sel.cursor_line, 1);
        sel.move_cursor_up();
        assert_eq!(sel.cursor_line, 1, "stays at first changeable line");
    }

    #[test]
    fn context_lines_not_selectable() {
        let mut sel = LineSelection::new(make_hunk_lines(&[
            HunkLineOrigin::Context,
            HunkLineOrigin::Deletion,
        ]));
        assert!(!sel.selected[0]);
        assert!(sel.selected[1]);
        sel.cursor_line = 0;
        sel.toggle_line();
        assert!(!sel.selected[0], "context line remains unselected");
    }
}
