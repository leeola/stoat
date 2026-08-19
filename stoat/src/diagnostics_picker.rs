use crate::{
    buffer::TextBuffer,
    diagnostics::DiagnosticSet,
    host::OffsetEncoding,
    input_view::InputView,
    picker::{Preview, PreviewSource, TargetPicker},
};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use std::{collections::HashMap, path::PathBuf};

/// Whether the picker lists the focused buffer's diagnostics
/// only (`Local`) or every workspace path (`Workspace`). The
/// renderer paints a path column when scope is `Workspace`,
/// and selecting a `Workspace` entry opens its file before
/// jumping.
///
/// A local entry carries no path of its own, which is what marks it local to
/// the jump. The scope holds the one path they all share, so the preview still
/// knows which file to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerScope {
    Local(PathBuf),
    Workspace,
}

/// Modal listing diagnostics in either the focused buffer
/// (Local scope) or every path in `Stoat::diagnostics`
/// (Workspace scope). Built from a snapshot of the diagnostic
/// set so render can run without re-entering buffer locks.
/// Selecting an entry collapses the focused editor's cursor at
/// its diagnostic. Workspace entries open the target file first.
///
/// Navigation, filtering, and selection route through the
/// `modal == diagnostics && mode == insert` keymap block. A query narrows the
/// list against [`diagnostic_haystack`], and [`Self::selected_entry`] reports
/// the surviving diagnostic to jump to.
pub struct DiagnosticsPicker {
    /// The filter, selection, and preview every target list shares.
    pub(crate) picker: TargetPicker<DiagnosticsEntry>,
    /// Which diagnostics the list was built from. Two open commands set this,
    /// and the title and the jump both read it.
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
    /// Wrap `entries` with the prompt and preview, tagged with `scope`.
    pub(crate) fn from_entries(
        entries: Vec<DiagnosticsEntry>,
        scope: PickerScope,
        input: InputView,
        preview: Preview,
    ) -> Self {
        let haystacks = entries.iter().map(diagnostic_haystack).collect();
        Self {
            picker: TargetPicker::new(entries, haystacks, input, preview),
            scope,
        }
    }

    pub fn scope(&self) -> &PickerScope {
        &self.scope
    }

    pub fn entries(&self) -> &[DiagnosticsEntry] {
        self.picker.entries()
    }

    pub(crate) fn filtered(&self) -> &[usize] {
        self.picker.filtered()
    }

    /// The diagnostic under the selection, or [`None`] when the filter matched
    /// nothing.
    pub(crate) fn selected_entry(&self) -> Option<&DiagnosticsEntry> {
        self.picker.selected_entry()
    }

    /// Cursor into the filtered rows, not into the entries. A caller wanting
    /// the entry itself takes [`Self::selected_entry`].
    pub(crate) fn selected(&self) -> usize {
        self.picker.selected()
    }

    pub(crate) fn page(&mut self, dir: i32) {
        self.picker.page(dir);
    }

    pub(crate) fn dispose(self, ws: &mut crate::workspace::Workspace) {
        self.picker.dispose(ws);
    }

    /// One entry per diagnostic in a buffer, each paired with the offset
    /// encoding of the server that published it.
    ///
    /// Each `range.start` becomes a byte offset through its server's encoding
    /// plus a `(line, column)` pair the position column shows. The message is
    /// truncated to [`MESSAGE_MAX_CHARS`] and stripped of embedded newlines so
    /// it fits a single row. Entries are sorted by `(line, column)` ascending.
    pub fn local_entries(
        diagnostics: &[(OffsetEncoding, Diagnostic)],
        buffer: &TextBuffer,
    ) -> Vec<DiagnosticsEntry> {
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
        entries
    }

    /// One entry per `(path, diagnostic)` pair in the workspace's diagnostic
    /// set.
    ///
    /// Every `offset` is a sentinel `0`, because the target file need not be
    /// open yet. The select handler recomputes the real byte offset after
    /// opening it. Entries are sorted by `(path, line, column)` so the list
    /// reads predictably.
    pub fn workspace_entries(
        diagnostics: &DiagnosticSet,
        encodings: &HashMap<String, OffsetEncoding>,
    ) -> Vec<DiagnosticsEntry> {
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
        entries
    }
}

/// What a query matches a diagnostic against. The haystack names where it is
/// and what it says, which is what the row shows.
fn diagnostic_haystack(entry: &DiagnosticsEntry) -> String {
    match &entry.path {
        Some(path) => format!("{}:{} {}", path.display(), entry.line, entry.message),
        None => format!("{} {}", entry.line, entry.message),
    }
}

/// Characters of [`diagnostic_haystack`] that come before the message.
///
/// The renderer highlights only the matched characters that fall in the message
/// column, so it subtracts this from every match offset.
pub(crate) fn haystack_prefix_len(entry: &DiagnosticsEntry) -> u32 {
    let haystack = diagnostic_haystack(entry);
    (haystack.chars().count() - entry.message.chars().count()) as u32
}

/// Where a diagnostic's preview reads from and which 0-based line it centres
/// on.
///
/// A workspace entry names its own file. A local one does not, so the scope's
/// path stands in for it. An open buffer wins over the file either way, so a
/// diagnostic in edited text previews what the reader has on screen.
pub(crate) fn diagnostic_target(
    ws: &crate::workspace::Workspace,
    scope: &PickerScope,
    entry: &DiagnosticsEntry,
) -> Option<(PreviewSource, u32)> {
    let path = match (&entry.path, scope) {
        (Some(path), _) => path,
        (None, PickerScope::Local(path)) => path,
        (None, PickerScope::Workspace) => return None,
    };
    let source = match ws.buffers.id_for_path(path) {
        Some(id) => PreviewSource::Buffer(id),
        None => PreviewSource::File(path.clone()),
    };
    Some((source, entry.line.saturating_sub(1)))
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
    use stoat_scheduler::{Executor, TestScheduler};

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
    fn local_entries_list_every_diagnostic_with_position() {
        let buffer = buf("alpha\nbeta\ngamma\n");
        let diagnostics = utf16(vec![
            diag(0, 0, "first", DiagnosticSeverity::ERROR),
            diag(2, 2, "third", DiagnosticSeverity::WARNING),
            diag(1, 1, "second", DiagnosticSeverity::INFORMATION),
        ]);
        let entries = DiagnosticsPicker::local_entries(&diagnostics, &buffer);
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.line, e.column, e.message.as_str()))
                .collect::<Vec<_>>(),
            [(1, 1, "first"), (2, 2, "second"), (3, 3, "third")],
            "entries sort by position"
        );
        assert!(entries.iter().all(|e| e.path.is_none()));
    }

    #[test]
    fn workspace_entries_list_pairs_from_every_path() {
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
        let entries = DiagnosticsPicker::workspace_entries(&set, &HashMap::new());
        assert_eq!(
            entries
                .iter()
                .map(|e| (
                    e.path.as_deref().map(|p| p.to_string_lossy().into_owned()),
                    e.message.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                (Some("/ws/a.rs".to_string()), "a-first"),
                (Some("/ws/a.rs".to_string()), "a-second"),
                (Some("/ws/b.rs".to_string()), "b-first"),
                (Some("/ws/b.rs".to_string()), "b-second"),
            ],
            "entries sort by path then position"
        );
        assert!(entries.iter().all(|e| e.offset == 0));
    }

    #[test]
    fn local_entries_truncate_long_messages_and_strip_newlines() {
        let buffer = buf("x\n");
        let long = "a".repeat(200);
        let multi = format!("first\nsecond\n{long}");
        let diagnostics = utf16(vec![diag(0, 0, &multi, DiagnosticSeverity::ERROR)]);
        let entries = DiagnosticsPicker::local_entries(&diagnostics, &buffer);
        assert_eq!(entries[0].message.chars().count(), MESSAGE_MAX_CHARS);
        assert!(!entries[0].message.contains('\n'));
    }

    /// The renderer subtracts this from a match offset to find the column in
    /// the message, so it has to count exactly what the haystack puts ahead of
    /// the message and no more.
    #[test]
    fn the_haystack_prefix_covers_everything_before_the_message() {
        let entry = |path: Option<&str>| DiagnosticsEntry {
            offset: 0,
            line: 12,
            column: 1,
            severity: None,
            message: "unresolved import".to_string(),
            path: path.map(PathBuf::from),
            encoding: OffsetEncoding::Utf16,
        };
        let local = entry(None);
        let workspace = entry(Some("/ws/a.rs"));
        assert_eq!(
            (haystack_prefix_len(&local), haystack_prefix_len(&workspace)),
            (3, 12),
            "`12 ` and `/ws/a.rs:12 ` are what precede the message"
        );
    }

    /// A local entry names no file, which is what marks its jump local. The
    /// preview still has to find one, so it falls back on the scope's path.
    #[test]
    fn a_local_target_reads_the_file_the_scope_names() {
        let executor = Executor::new(std::sync::Arc::new(TestScheduler::new()));
        let ws =
            crate::workspace::Workspace::new(PathBuf::from("/ws"), &executor, crate::test_notify());
        let entry = DiagnosticsEntry {
            offset: 0,
            line: 12,
            column: 1,
            severity: None,
            message: "unresolved import".to_string(),
            path: None,
            encoding: OffsetEncoding::Utf16,
        };

        let local = diagnostic_target(&ws, &PickerScope::Local(PathBuf::from("/ws/a.rs")), &entry);
        assert!(
            matches!(
                local,
                Some((PreviewSource::File(ref path), 11)) if path == std::path::Path::new("/ws/a.rs")
            ),
            "the scope's file previews at the diagnostic's 0-based line"
        );
        assert!(
            diagnostic_target(&ws, &PickerScope::Workspace, &entry).is_none(),
            "a workspace entry without a path of its own has nothing to read"
        );
    }
}
