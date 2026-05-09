use crate::buffer::TextBuffer;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use stoat_text::Point;

/// Modal listing every diagnostic for the focused buffer. Built
/// from a snapshot of `Stoat::diagnostics` and the buffer's
/// rope so render and key dispatch can run without re-entering
/// buffer locks. Selection collapses the cursor at the entry's
/// `offset`.
pub struct DiagnosticsPicker {
    entries: Vec<DiagnosticsEntry>,
    selected: usize,
    pub previous_mode: String,
}

pub struct DiagnosticsEntry {
    pub offset: usize,
    pub line: u32,
    pub column: u32,
    pub severity: Option<DiagnosticSeverity>,
    pub message: String,
}

pub enum PickerOutcome {
    None,
    Close,
    Select(usize),
}

const MESSAGE_MAX_CHARS: usize = 80;

impl DiagnosticsPicker {
    /// Build a picker from a buffer's diagnostic list. Each
    /// diagnostic's `range.start` is converted to a byte offset
    /// (clamped to the rope's length) plus a `(line, column)`
    /// pair shown in the position column. The message is
    /// truncated to [`MESSAGE_MAX_CHARS`] and stripped of any
    /// embedded newlines so it fits the single-row layout.
    /// Entries are sorted by `(line, column)` ascending.
    pub fn new(diagnostics: &[Diagnostic], buffer: &TextBuffer, previous_mode: String) -> Self {
        let rope = buffer.rope();
        let rope_len = rope.len();
        let mut entries: Vec<DiagnosticsEntry> = diagnostics
            .iter()
            .map(|diag| {
                let line = diag.range.start.line;
                let column = diag.range.start.character;
                let point = Point::new(line, column);
                let offset = rope.point_to_offset(point).min(rope_len);
                let message: String = diag
                    .message
                    .replace('\n', " ")
                    .chars()
                    .take(MESSAGE_MAX_CHARS)
                    .collect();
                DiagnosticsEntry {
                    offset,
                    line: line + 1,
                    column: column + 1,
                    severity: diag.severity,
                    message,
                }
            })
            .collect();
        entries.sort_by_key(|e| (e.line, e.column));
        Self {
            entries,
            selected: 0,
            previous_mode,
        }
    }

    pub fn entries(&self) -> &[DiagnosticsEntry] {
        &self.entries
    }

    pub fn selected(&self) -> usize {
        self.selected
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
    use lsp_types::{Position, Range};

    fn buf(text: &str) -> TextBuffer {
        TextBuffer::with_text(BufferId::new(1), text)
    }

    fn diag(line: u32, column: u32, message: &str, severity: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line,
                    character: column,
                },
                end: Position {
                    line,
                    character: column + 1,
                },
            },
            severity: Some(severity),
            code: None,
            code_description: None,
            source: None,
            message: message.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    #[test]
    fn new_lists_every_diagnostic_with_position() {
        let buffer = buf("alpha\nbeta\ngamma\n");
        let diagnostics = vec![
            diag(0, 0, "first", DiagnosticSeverity::ERROR),
            diag(2, 2, "third", DiagnosticSeverity::WARNING),
            diag(1, 1, "second", DiagnosticSeverity::INFORMATION),
        ];
        let picker = DiagnosticsPicker::new(&diagnostics, &buffer, "normal".into());
        let entries = picker.entries();
        assert_eq!(entries.len(), 3);
        assert_eq!((entries[0].line, entries[0].column), (1, 1));
        assert_eq!(entries[0].message, "first");
        assert_eq!((entries[1].line, entries[1].column), (2, 2));
        assert_eq!(entries[1].message, "second");
        assert_eq!((entries[2].line, entries[2].column), (3, 3));
        assert_eq!(entries[2].message, "third");
    }

    #[test]
    fn new_truncates_long_messages_and_strips_newlines() {
        let buffer = buf("x\n");
        let long = "a".repeat(200);
        let multi = format!("first\nsecond\n{long}");
        let diagnostics = vec![diag(0, 0, &multi, DiagnosticSeverity::ERROR)];
        let picker = DiagnosticsPicker::new(&diagnostics, &buffer, "normal".into());
        let entry = &picker.entries()[0];
        assert_eq!(entry.message.chars().count(), MESSAGE_MAX_CHARS);
        assert!(!entry.message.contains('\n'));
    }

    #[test]
    fn enter_returns_select() {
        let buffer = buf("a\n");
        let diagnostics = vec![diag(0, 0, "msg", DiagnosticSeverity::ERROR)];
        let mut picker = DiagnosticsPicker::new(&diagnostics, &buffer, "normal".into());
        assert!(matches!(
            picker.handle_key(keys::key(KeyCode::Enter)),
            PickerOutcome::Select(_)
        ));
    }

    #[test]
    fn esc_returns_close() {
        let buffer = buf("a\n");
        let diagnostics = vec![diag(0, 0, "msg", DiagnosticSeverity::ERROR)];
        let mut picker = DiagnosticsPicker::new(&diagnostics, &buffer, "normal".into());
        assert!(matches!(
            picker.handle_key(keys::key(KeyCode::Esc)),
            PickerOutcome::Close
        ));
    }

    #[test]
    fn down_and_up_clamp_at_ends() {
        let buffer = buf("a\nb\nc\n");
        let diagnostics = vec![
            diag(0, 0, "first", DiagnosticSeverity::ERROR),
            diag(1, 0, "second", DiagnosticSeverity::ERROR),
            diag(2, 0, "third", DiagnosticSeverity::ERROR),
        ];
        let mut picker = DiagnosticsPicker::new(&diagnostics, &buffer, "normal".into());
        picker.handle_key(keys::key(KeyCode::Up));
        picker.handle_key(keys::key(KeyCode::Up));
        assert_eq!(picker.selected(), 0);
        picker.handle_key(keys::key(KeyCode::Down));
        picker.handle_key(keys::key(KeyCode::Down));
        picker.handle_key(keys::key(KeyCode::Down));
        assert_eq!(picker.selected(), 2);
    }
}
