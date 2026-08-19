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
    /// Rows the last render painted, stamped by the renderer and read by
    /// [`Self::page`]. `None` until the first frame, since the modal sizes
    /// itself to its entries and only render knows how many fit.
    pub(crate) viewport_rows: Option<usize>,
}

impl LocationPicker {
    pub(crate) fn new(entries: Vec<LocationEntry>) -> Self {
        Self {
            entries,
            selected: 0,
            viewport_rows: None,
        }
    }

    pub(crate) fn entries(&self) -> &[LocationEntry] {
        &self.entries
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    /// Page the selection by half the rendered list height in `dir` (negative
    /// up, positive down). Falls back to a single row before the first render
    /// sets [`Self::viewport_rows`].
    pub(crate) fn page(&mut self, dir: i32) {
        self.move_selection(dir * crate::picker::nav_page_step(self.viewport_rows));
    }

    /// Move the selection to `index`, clamped to the last entry so a stale row
    /// number from a hit test cannot select past the list.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        crate::picker::nav_move(self.entries.len(), &mut self.selected, delta);
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
        picker.move_selection(1);
        assert_eq!(picker.selected(), 1);
        picker.move_selection(-1);
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn navigation_clamps_within_bounds() {
        let mut picker = LocationPicker::new(vec![entry(1), entry(2)]);
        picker.move_selection(-1);
        assert_eq!(picker.selected(), 0);
        picker.move_selection(1);
        picker.move_selection(1);
        assert_eq!(picker.selected(), 1);
    }
}
