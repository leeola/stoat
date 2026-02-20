use crate::{
    git::{
        conflict::{parse_conflicts, ConflictRegion},
        diff::BufferDiff,
    },
    index::{
        bracket::{BracketIndex, BracketSnapshot},
        scope::{ScopeIndex, ScopeSnapshot},
        symbol::{SymbolIndex, SymbolSnapshot},
        SyntaxIndex,
    },
    syntax::{HighlightMap, SyntaxTheme},
};
use gpui::{App, Context, Entity, EventEmitter};
use smallvec::SmallVec;
use std::{
    ops::Range,
    time::{Instant, SystemTime},
};
use stoat_lsp::{BufferDiagnostic, DiagnosticSet, ServerId};
use stoat_text::{HighlightCapture, Language, Parser};
use text::{Buffer, BufferSnapshot, LineEnding};

pub enum BufferItemEvent {
    DiagnosticsUpdated,
}

pub struct BufferItem {
    buffer: Entity<Buffer>,
    parser: Parser,
    language: Language,
    highlight_map: Option<HighlightMap>,
    diff: Option<BufferDiff>,
    staged_rows: Option<Vec<Range<u32>>>,
    staged_hunk_indices: Option<Vec<usize>>,
    saved_text: Option<String>,
    saved_mtime: Option<SystemTime>,
    line_ending: LineEnding,
    symbol_index: Option<SymbolIndex>,
    scope_index: Option<ScopeIndex>,
    bracket_index: Option<BracketIndex>,
    conflicts: Vec<ConflictRegion>,
    diagnostics: SmallVec<[(ServerId, DiagnosticSet); 2]>,
    diagnostics_version: u64,
}

impl EventEmitter<BufferItemEvent> for BufferItem {}

impl BufferItem {
    pub fn new(buffer: Entity<Buffer>, language: Language, _cx: &App) -> Self {
        let parser = Parser::new(language).expect("Failed to create parser");

        Self {
            buffer,
            parser,
            language,
            highlight_map: None,
            diff: None,
            staged_rows: None,
            staged_hunk_indices: None,
            saved_text: None,
            saved_mtime: None,
            line_ending: LineEnding::default(),
            symbol_index: None,
            scope_index: None,
            bracket_index: None,
            conflicts: Vec::new(),
            diagnostics: SmallVec::new(),
            diagnostics_version: 0,
        }
    }

    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }

    pub fn buffer_snapshot(&self, cx: &App) -> BufferSnapshot {
        self.buffer.read(cx).snapshot()
    }

    pub fn display_buffer(
        &self,
        cx: &App,
        show_phantom_rows: bool,
        comparison_mode: Option<crate::git::diff_review::DiffComparisonMode>,
    ) -> crate::DisplayBuffer {
        crate::DisplayBuffer::new(
            self.buffer_snapshot(cx),
            self.diff.clone(),
            show_phantom_rows,
            self.staged_rows.as_deref(),
            self.staged_hunk_indices.as_deref(),
            comparison_mode,
        )
    }

    /// Compute highlight captures for a byte range of the buffer.
    ///
    /// Runs the tree-sitter highlight query against the current parse tree.
    /// Returns an empty vec if no tree or highlight query is available.
    pub fn highlight_captures(&self, range: Range<usize>, source: &str) -> Vec<HighlightCapture> {
        let tree = match self.parser.tree() {
            Some(t) => t,
            None => return vec![],
        };
        let query = match self.parser.highlight_query() {
            Some(q) => q,
            None => return vec![],
        };
        query.captures(tree, source.as_bytes(), range)
    }

    pub fn highlight_map(&self) -> Option<&HighlightMap> {
        self.highlight_map.as_ref()
    }

    pub fn symbol_snapshot(&self) -> Option<SymbolSnapshot> {
        self.symbol_index.as_ref().map(|i| i.snapshot())
    }

    pub fn scope_snapshot(&self) -> Option<ScopeSnapshot> {
        self.scope_index.as_ref().map(|i| i.snapshot())
    }

    pub fn bracket_snapshot(&self) -> Option<BracketSnapshot> {
        self.bracket_index.as_ref().map(|i| i.snapshot())
    }

    pub fn language(&self) -> Language {
        self.language
    }

    fn rebuild_indices(&mut self, source: &str, buffer: &BufferSnapshot) {
        if let Some(tree) = self.parser.tree() {
            self.symbol_index = Some(SymbolIndex::rebuild(tree, source, buffer, self.language));
            self.scope_index = Some(ScopeIndex::rebuild(tree, source, buffer, self.language));
            self.bracket_index = Some(BracketIndex::rebuild(tree, source, buffer, self.language));
        }
    }

    fn ensure_highlight_map(&mut self, theme: &SyntaxTheme) {
        if self.highlight_map.is_none() {
            if let Some(query) = self.parser.highlight_query() {
                self.highlight_map = Some(HighlightMap::new(theme, query.capture_names()));
            }
        }
    }

    pub fn reparse(&mut self, cx: &App) -> Result<(), String> {
        let start = Instant::now();

        let contents = self.buffer.read(cx).text();
        let text_time = start.elapsed();

        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let snapshot_time = start.elapsed() - text_time;

        let parse_start = Instant::now();
        match self.parser.parse(&contents) {
            Ok(()) => {
                let parse_time = parse_start.elapsed();

                self.ensure_highlight_map(&SyntaxTheme::default());
                self.rebuild_indices(&contents, &buffer_snapshot);

                let total = start.elapsed();
                tracing::debug!(
                    "reparse: total={:?} (text={:?}, snapshot={:?}, parse={:?}) bytes={}",
                    total,
                    text_time,
                    snapshot_time,
                    parse_time,
                    contents.len()
                );
                Ok(())
            },
            Err(e) => {
                tracing::debug!("Failed to parse buffer: {}", e);
                Err(format!("Parse error: {e}"))
            },
        }
    }

    pub fn reparse_incremental(
        &mut self,
        edits: &[text::Edit<usize>],
        cx: &App,
    ) -> Result<(), String> {
        let start = Instant::now();
        let contents = self.buffer.read(cx).text();
        let buffer_snapshot = self.buffer.read(cx).snapshot();

        match self
            .parser
            .parse_incremental(&contents, &buffer_snapshot, edits)
        {
            Ok(_changed_ranges) => {
                let parse_time = start.elapsed();

                self.ensure_highlight_map(&SyntaxTheme::default());
                self.rebuild_indices(&contents, &buffer_snapshot);

                tracing::debug!(
                    "reparse_incremental: total={:?} (parse={:?})",
                    start.elapsed(),
                    parse_time,
                );

                Ok(())
            },
            Err(e) => {
                tracing::debug!(
                    "Incremental parse failed, falling back to full reparse: {}",
                    e
                );
                self.reparse(cx)
            },
        }
    }

    pub fn set_language(&mut self, language: Language) {
        if language != self.language {
            self.language = language;
            self.parser = Parser::new(language).expect("Failed to create parser");
            self.highlight_map = None;
        }
    }

    pub fn diff(&self) -> Option<&BufferDiff> {
        self.diff.as_ref()
    }

    pub fn set_diff(&mut self, diff: Option<BufferDiff>) {
        self.diff = diff;
    }

    pub fn staged_rows(&self) -> Option<&[Range<u32>]> {
        self.staged_rows.as_deref()
    }

    pub fn set_staged_rows(&mut self, staged_rows: Option<Vec<Range<u32>>>) {
        self.staged_rows = staged_rows;
    }

    pub fn staged_hunk_indices(&self) -> Option<&[usize]> {
        self.staged_hunk_indices.as_deref()
    }

    pub fn set_staged_hunk_indices(&mut self, indices: Option<Vec<usize>>) {
        self.staged_hunk_indices = indices;
    }

    pub fn conflicts(&self) -> &[ConflictRegion] {
        &self.conflicts
    }

    pub fn reparse_conflicts(&mut self, cx: &App) {
        let text = self.buffer.read(cx).text();
        self.conflicts = parse_conflicts(&text);
    }

    pub fn is_modified(&self, cx: &App) -> bool {
        if let Some(saved) = &self.saved_text {
            let current = self.buffer.read(cx).text();
            current != *saved
        } else {
            false
        }
    }

    pub fn set_saved_text(&mut self, text: String) {
        self.saved_text = Some(text);
    }

    pub fn set_saved_mtime(&mut self, mtime: SystemTime) {
        self.saved_mtime = Some(mtime);
    }

    pub fn saved_mtime(&self) -> Option<SystemTime> {
        self.saved_mtime
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn set_line_ending(&mut self, line_ending: LineEnding) {
        self.line_ending = line_ending;
    }

    pub fn has_conflict(&self, file_path: &std::path::Path, cx: &App) -> bool {
        if !self.is_modified(cx) {
            return false;
        }

        let Some(saved_mtime) = self.saved_mtime else {
            return false;
        };

        let Ok(metadata) = std::fs::metadata(file_path) else {
            return false;
        };

        let Ok(current_mtime) = metadata.modified() else {
            return false;
        };

        current_mtime > saved_mtime
    }

    pub fn base_text_for_hunk(&self, hunk_idx: usize) -> &str {
        self.diff
            .as_ref()
            .map(|d| d.base_text_for_hunk(hunk_idx))
            .unwrap_or("")
    }

    pub fn update_diagnostics(
        &mut self,
        server_id: ServerId,
        diagnostics: DiagnosticSet,
        version: u64,
        cx: &mut Context<Self>,
    ) {
        if version > self.diagnostics_version {
            if let Some(pos) = self.diagnostics.iter().position(|(id, _)| *id == server_id) {
                self.diagnostics[pos].1 = diagnostics;
            } else {
                self.diagnostics.push((server_id, diagnostics));
            }
            self.diagnostics_version = version;

            cx.notify();
            cx.emit(BufferItemEvent::DiagnosticsUpdated);
        }
    }

    pub fn clear_diagnostics(&mut self, server_id: ServerId, cx: &mut Context<Self>) {
        if let Some(pos) = self.diagnostics.iter().position(|(id, _)| *id == server_id) {
            self.diagnostics.remove(pos);
            cx.notify();
            cx.emit(BufferItemEvent::DiagnosticsUpdated);
        }
    }

    pub fn diagnostics_for_row<'a>(
        &'a self,
        row: u32,
        snapshot: &'a BufferSnapshot,
    ) -> impl Iterator<Item = &'a BufferDiagnostic> + 'a {
        self.diagnostics
            .iter()
            .flat_map(move |(_, diag_set)| diag_set.diagnostics_for_row(row, snapshot))
    }
}
