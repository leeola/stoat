//! Multi-location goto picker.
//!
//! Opened by [`crate::action_handlers::lsp::pump_lsp_jumps`] when a goto
//! request resolves to more than one location. Presents the candidates
//! as `path:line:col  target-line` rows. Enter jumps to the selected one
//! through the same apply path a single-location goto uses.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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

/// The action [`LocationPicker::handle_key`] resolved a key into. `Select`
/// carries the row index. The dispatcher re-reads `entries()[idx]`.
pub(crate) enum PickerOutcome {
    None,
    Close,
    Select(usize),
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

    pub(crate) fn hint_bindings(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Enter", "jump".to_string()),
            ("Esc", "cancel".to_string()),
            ("Ctrl-N", "next".to_string()),
            ("Ctrl-P", "prev".to_string()),
        ]
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
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
    use crate::test_harness::keys::key;

    fn entry(line: u32) -> LocationEntry {
        LocationEntry {
            path: PathBuf::from("/ws/a.rs"),
            offset: 0,
            line,
            column: 1,
            text: String::new(),
        }
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn enter_selects_the_highlighted_row() {
        let mut picker = LocationPicker::new(vec![entry(1), entry(2), entry(3)]);
        picker.handle_key(ctrl(KeyCode::Char('n')));
        assert!(matches!(
            picker.handle_key(key(KeyCode::Enter)),
            PickerOutcome::Select(1)
        ));
    }

    #[test]
    fn navigation_clamps_within_bounds() {
        let mut picker = LocationPicker::new(vec![entry(1), entry(2)]);
        picker.handle_key(ctrl(KeyCode::Char('p')));
        assert_eq!(picker.selected(), 0);
        picker.handle_key(ctrl(KeyCode::Char('n')));
        picker.handle_key(ctrl(KeyCode::Char('n')));
        assert_eq!(picker.selected(), 1);
        assert!(matches!(
            picker.handle_key(key(KeyCode::Esc)),
            PickerOutcome::Close
        ));
    }
}
