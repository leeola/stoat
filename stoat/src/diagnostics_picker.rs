use crate::{buffer::TextBuffer, diagnostics::DiagnosticSet};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use std::path::PathBuf;
use stoat_text::Point;

/// Whether the picker lists the focused buffer's diagnostics
/// only (`Local`) or every workspace path (`Workspace`). The
/// renderer paints a path column when scope is `Workspace`,
/// and the dispatch arm opens the entry's file before jumping
/// when scope is `Workspace`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerScope {
    Local,
    Workspace,
}

/// Modal listing diagnostics in either the focused buffer
/// (Local scope) or every path in `Stoat::diagnostics`
/// (Workspace scope). Built from a snapshot of the diagnostic
/// set so render and key dispatch can run without re-entering
/// buffer locks. Selection collapses the focused editor's
/// cursor at the entry's diagnostic; workspace entries open
/// the target file first.
pub struct DiagnosticsPicker {
    entries: Vec<DiagnosticsEntry>,
    selected: usize,
    scope: PickerScope,
    pub previous_mode: String,
}

pub struct DiagnosticsEntry {
    /// Byte offset in the entry's source buffer. Meaningful
    /// only for Local entries -- workspace entries set this to
    /// 0 because the target buffer may not be open at picker
    /// construction time. The dispatcher recomputes the offset
    /// from `(line, column)` after opening the file.
    pub offset: usize,
    pub line: u32,
    pub column: u32,
    pub severity: Option<DiagnosticSeverity>,
    pub message: String,
    /// Absolute path of the file the diagnostic comes from.
    /// `None` for Local-scope entries (caller already has the
    /// focused editor's path); `Some` for Workspace-scope
    /// entries.
    pub path: Option<PathBuf>,
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
                DiagnosticsEntry {
                    offset,
                    line: line + 1,
                    column: column + 1,
                    severity: diag.severity,
                    message: render_message(&diag.message),
                    path: None,
                }
            })
            .collect();
        entries.sort_by_key(|e| (e.line, e.column));
        Self {
            entries,
            selected: 0,
            scope: PickerScope::Local,
            previous_mode,
        }
    }

    /// Build a picker over every `(path, diagnostic)` pair in
    /// the workspace's diagnostic set. The `offset` field on
    /// each entry is a sentinel `0`; the dispatch arm
    /// recomputes the real byte offset after opening the
    /// target file. Entries are sorted by `(path, line,
    /// column)` so the picker reads predictably.
    pub fn workspace(diagnostics: &DiagnosticSet, previous_mode: String) -> Self {
        let mut entries: Vec<DiagnosticsEntry> = diagnostics
            .iter()
            .flat_map(|(path, diags)| {
                let path = path.to_path_buf();
                diags.iter().map(move |diag| {
                    let line = diag.range.start.line;
                    let column = diag.range.start.character;
                    DiagnosticsEntry {
                        offset: 0,
                        line: line + 1,
                        column: column + 1,
                        severity: diag.severity,
                        message: render_message(&diag.message),
                        path: Some(path.clone()),
                    }
                })
            })
            .collect();
        entries.sort_by(|a, b| {
            let a_path = a.path.as_deref();
            let b_path = b.path.as_deref();
            a_path
                .cmp(&b_path)
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.column.cmp(&b.column))
        });
        Self {
            entries,
            selected: 0,
            scope: PickerScope::Workspace,
            previous_mode,
        }
    }

    pub fn scope(&self) -> PickerScope {
        self.scope
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

fn render_message(raw: &str) -> String {
    raw.replace('\n', " ")
        .chars()
        .take(MESSAGE_MAX_CHARS)
        .collect()
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
        assert_eq!(picker.scope(), PickerScope::Local);
        assert!(entries.iter().all(|e| e.path.is_none()));
    }

    #[test]
    fn workspace_lists_pairs_from_every_path() {
        use std::path::PathBuf;
        let mut set = DiagnosticSet::new();
        set.replace_for_path(
            PathBuf::from("/ws/a.rs"),
            vec![diag(2, 0, "a-second", DiagnosticSeverity::ERROR)],
        );
        set.replace_for_path(
            PathBuf::from("/ws/b.rs"),
            vec![
                diag(1, 0, "b-second", DiagnosticSeverity::WARNING),
                diag(0, 0, "b-first", DiagnosticSeverity::ERROR),
            ],
        );
        set.replace_for_path(
            PathBuf::from("/ws/a.rs"),
            vec![
                diag(0, 0, "a-first", DiagnosticSeverity::ERROR),
                diag(2, 0, "a-second", DiagnosticSeverity::ERROR),
            ],
        );
        let picker = DiagnosticsPicker::workspace(&set, "normal".into());
        assert_eq!(picker.scope(), PickerScope::Workspace);
        let entries = picker.entries();
        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries[0].path.as_deref(),
            Some(std::path::Path::new("/ws/a.rs"))
        );
        assert_eq!(entries[0].message, "a-first");
        assert_eq!(entries[1].message, "a-second");
        assert_eq!(
            entries[2].path.as_deref(),
            Some(std::path::Path::new("/ws/b.rs"))
        );
        assert_eq!(entries[2].message, "b-first");
        assert_eq!(entries[3].message, "b-second");
        assert!(entries.iter().all(|e| e.offset == 0));
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
