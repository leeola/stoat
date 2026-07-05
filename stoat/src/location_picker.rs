//! Multi-location goto picker.
//!
//! Opened by [`crate::action_handlers::lsp::pump_lsp_jumps`] when a goto
//! request resolves to more than one location. Presents the candidates
//! as `path:line:col  target-line` rows. Navigation and selection route
//! through the `modal == location` keymap block. Selecting a row jumps
//! through the same apply path a single-location goto uses.

use std::path::PathBuf;

/// One resolved goto candidate. Carries the byte offset to jump to plus
/// the 1-based line/column and the target line's text for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocationEntry {
    pub(crate) path: PathBuf,
    pub(crate) offset: usize,
    pub(crate) line: u32,
    pub(crate) column: u32,
    pub(crate) text: String,
}

/// Modal chooser over the candidates of a multi-location goto.
pub(crate) struct LocationPicker {
    entries: Vec<LocationEntry>,
    selected: usize,
}

impl LocationPicker {
    pub(crate) fn new(entries: Vec<LocationEntry>) -> Self {
        Self {
            entries,
            selected: 0,
        }
    }

    pub(crate) fn entries(&self) -> &[LocationEntry] {
        &self.entries
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn select_next(&mut self) {
        self.move_selection(1);
    }

    pub(crate) fn select_prev(&mut self) {
        self.move_selection(-1);
    }

    pub(crate) fn hint_bindings(&self) -> Vec<(&'static str, String)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(line: u32) -> LocationEntry {
        LocationEntry {
            path: PathBuf::from("/ws/a.rs"),
            offset: 0,
            line,
            column: 1,
            text: String::new(),
        }
    }

    #[test]
    fn select_next_prev_track_selection() {
        let mut picker = LocationPicker::new(vec![entry(1), entry(2), entry(3)]);
        picker.select_next();
        assert_eq!(picker.selected(), 1);
        picker.select_prev();
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn navigation_clamps_within_bounds() {
        let mut picker = LocationPicker::new(vec![entry(1), entry(2)]);
        picker.select_prev();
        assert_eq!(picker.selected(), 0);
        picker.select_next();
        picker.select_next();
        assert_eq!(picker.selected(), 1);
    }
}
