use crate::{buffer::TextBuffer, diagnostics::DiagnosticSet, host::OffsetEncoding};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use std::{collections::HashMap, path::PathBuf};

/// Whether the picker lists the focused buffer's diagnostics
/// only (`Local`) or every workspace path (`Workspace`). The
/// renderer paints a path column when scope is `Workspace`,
/// and selecting a `Workspace` entry opens its file before
/// jumping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerScope {
    Local,
    Workspace,
}

/// Modal listing diagnostics in either the focused buffer
/// (Local scope) or every path in `Stoat::diagnostics`
/// (Workspace scope). Built from a snapshot of the diagnostic
/// set so render can run without re-entering buffer locks.
/// Selecting an entry collapses the focused editor's cursor at
/// its diagnostic. Workspace entries open the target file first.
///
/// Navigation and selection route through the `modal == diagnostics`
/// keymap block. [`Self::select_next`] and [`Self::select_prev`] move
/// the highlight, and [`Self::selected`] reports the row to jump to.
pub struct DiagnosticsPicker {
    entries: Vec<DiagnosticsEntry>,
    selected: usize,
    scope: PickerScope,
}

pub struct DiagnosticsEntry {
    /// Byte offset in the entry's source buffer. Meaningful
    /// only for Local entries -- workspace entries set this to
    /// 0 because the target buffer may not be open at picker
    /// construction time. The select handler recomputes the
    /// offset from `(line, column)` after opening the file.
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
    /// Offset encoding of the server that published the diagnostic. The select
    /// handler converts `(line, column)` back to a byte offset through it, so a
    /// utf-16 server's column lands on the right byte of a multibyte line.
    pub encoding: OffsetEncoding,
}

const MESSAGE_MAX_CHARS: usize = 80;

impl DiagnosticsPicker {
    /// Build a picker from a buffer's diagnostics, each paired with the offset
    /// encoding of the server that published it.
    ///
    /// Each `range.start` is converted to a byte offset through its server's
    /// encoding plus a `(line, column)` pair shown in the position column. The
    /// message is truncated to [`MESSAGE_MAX_CHARS`] and stripped of any embedded
    /// newlines so it fits the single-row layout. Entries are sorted by
    /// `(line, column)` ascending.
    pub fn new(diagnostics: &[(OffsetEncoding, Diagnostic)], buffer: &TextBuffer) -> Self {
        let rope = buffer.rope();
        let mut entries: Vec<DiagnosticsEntry> = diagnostics
            .iter()
            .map(|(encoding, diag)| {
                let line = diag.range.start.line;
                let column = diag.range.start.character;
                let offset =
                    crate::lsp::util::lsp_pos_to_byte_offset(rope, diag.range.start, *encoding);
                DiagnosticsEntry {
                    offset,
                    line: line + 1,
                    column: column + 1,
                    severity: diag.severity,
                    message: render_message(&diag.message),
                    path: None,
                    encoding: *encoding,
                }
            })
            .collect();
        entries.sort_by_key(|e| (e.line, e.column));
        Self {
            entries,
            selected: 0,
            scope: PickerScope::Local,
        }
    }

    /// Build a picker over every `(path, diagnostic)` pair in
    /// the workspace's diagnostic set. The `offset` field on
    /// each entry is a sentinel `0`; the dispatch arm
    /// recomputes the real byte offset after opening the
    /// target file. Entries are sorted by `(path, line,
    /// column)` so the picker reads predictably.
    pub fn workspace(
        diagnostics: &DiagnosticSet,
        encodings: &HashMap<String, OffsetEncoding>,
    ) -> Self {
        let mut entries: Vec<DiagnosticsEntry> = diagnostics
            .iter_attributed()
            .map(|(path, server, diag)| {
                let line = diag.range.start.line;
                let column = diag.range.start.character;
                DiagnosticsEntry {
                    offset: 0,
                    line: line + 1,
                    column: column + 1,
                    severity: diag.severity,
                    message: render_message(&diag.message),
                    path: Some(path.to_path_buf()),
                    encoding: encodings
                        .get(server)
                        .copied()
                        .unwrap_or(OffsetEncoding::Utf16),
                }
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

fn render_message(raw: &str) -> String {
    raw.replace('\n', " ")
        .chars()
        .take(MESSAGE_MAX_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::BufferId;
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

    /// Pair each diagnostic with UTF-16, the default the picker assumes when a
    /// server is absent from the encoding map. These tests use ASCII content, so
    /// UTF-16 and UTF-8 resolve to the same byte offsets.
    fn utf16(diags: Vec<Diagnostic>) -> Vec<(OffsetEncoding, Diagnostic)> {
        diags
            .into_iter()
            .map(|d| (OffsetEncoding::Utf16, d))
            .collect()
    }

    #[test]
    fn new_lists_every_diagnostic_with_position() {
        let buffer = buf("alpha\nbeta\ngamma\n");
        let diagnostics = utf16(vec![
            diag(0, 0, "first", DiagnosticSeverity::ERROR),
            diag(2, 2, "third", DiagnosticSeverity::WARNING),
            diag(1, 1, "second", DiagnosticSeverity::INFORMATION),
        ]);
        let picker = DiagnosticsPicker::new(&diagnostics, &buffer);
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
        let picker = DiagnosticsPicker::workspace(&set, &HashMap::new());
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
        let diagnostics = utf16(vec![diag(0, 0, &multi, DiagnosticSeverity::ERROR)]);
        let picker = DiagnosticsPicker::new(&diagnostics, &buffer);
        let entry = &picker.entries()[0];
        assert_eq!(entry.message.chars().count(), MESSAGE_MAX_CHARS);
        assert!(!entry.message.contains('\n'));
    }

    #[test]
    fn select_next_prev_clamp_at_ends() {
        let buffer = buf("a\nb\nc\n");
        let diagnostics = utf16(vec![
            diag(0, 0, "first", DiagnosticSeverity::ERROR),
            diag(1, 0, "second", DiagnosticSeverity::ERROR),
            diag(2, 0, "third", DiagnosticSeverity::ERROR),
        ]);
        let mut picker = DiagnosticsPicker::new(&diagnostics, &buffer);
        picker.select_prev();
        picker.select_prev();
        assert_eq!(picker.selected(), 0);
        picker.select_next();
        picker.select_next();
        picker.select_next();
        assert_eq!(picker.selected(), 2);
    }
}
