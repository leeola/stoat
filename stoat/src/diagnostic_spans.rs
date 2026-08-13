//! Diagnostics resolved from what a language server published to where the text
//! is now, and the queries the editor asks of them.
//!
//! A server publishes a diagnostic against the text it last saw, and the reader
//! edits that text afterward. [`crate::diagnostics`] anchors each published
//! range so it follows its text. This module resolves those anchors to byte
//! offsets once per (set, buffer) version and caches the result, so the
//! per-frame paint and the mouse both binary-search a settled slice instead of
//! re-resolving.
//!
//! The paint is one caller among several. Nothing here draws.

use crate::editor_state::EditorState;
use lsp_types::{DiagnosticSeverity, DiagnosticTag};
use std::{ops::Range, path::Path};
use stoat_text::Anchor;

/// A diagnostic resolved to byte offsets once per (set, buffer) version, so the
/// per-frame render paths binary-search a cached slice instead of re-resolving
/// and re-scanning the whole list every frame.
///
/// `index` is the position in `set.get(path)`, so a consumer can recover the
/// original diagnostic (its message, tags) after locating a span. `start_line`/
/// `end_line` are the diagnostic's LSP rows, kept so the cursor-line query stays
/// line-based rather than reinterpreting byte ranges at line boundaries.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResolvedDiag {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) severity: DiagnosticSeverity,
    pub(crate) unnecessary: bool,
    pub(crate) start_line: u32,
    pub(crate) end_line: u32,
    pub(crate) index: usize,
}

/// Per-editor cache of [`ResolvedDiag`]s, rebuilt when the diagnostic set or the
/// buffer version changes. Transient render state, not persisted.
pub(crate) struct DiagnosticSpanCache {
    set_version: u64,
    buffer_version: u64,
    pub(crate) spans: Vec<ResolvedDiag>,
    /// Running maximum of [`Self::spans`]' ends, one entry per span.
    ///
    /// The spans are sorted by start, so their ends are not, and a viewport's
    /// lower bound cannot be searched for directly. This is non-decreasing by
    /// construction, and entry `i` at or below an offset proves every span up to
    /// `i` ends at or before it, which is what makes the bound searchable.
    prefix_max_end: Vec<usize>,
    /// Running maximum of [`Self::spans`]' end rows, one entry per span.
    ///
    /// The row counterpart of [`Self::prefix_max_end`], and searchable for the
    /// same reason. The cursor-line query is line-keyed rather than
    /// offset-keyed, and the two do not agree at a line's first byte: a
    /// diagnostic whose range ends at column 0 of a line counts as reaching that
    /// line, while its end offset is the byte before the line's own span. A
    /// bound in rows reaches exactly what a filter in rows accepts.
    prefix_max_end_line: Vec<u32>,
    /// The cursor line the readout last answered for, and the diagnostic it
    /// found there.
    ///
    /// The readout runs on every frame the cursor sits inside a diagnostic, and
    /// the answer only moves when the cursor changes line. The two versions
    /// above are not part of the key, since a change to either replaces this
    /// whole cache and the memo along with it.
    cursor_line_diag: Option<(u32, Option<ResolvedDiag>)>,
}

impl DiagnosticSpanCache {
    /// The index range of [`Self::spans`] that can overlap `visible`.
    ///
    /// Spans outside it are settled, so a caller still filters the ones inside
    /// for a real overlap.
    pub(crate) fn overlapping(&self, visible: Range<usize>) -> Range<usize> {
        let lo = self.prefix_max_end.partition_point(|&e| e <= visible.start);
        let hi = self.spans.partition_point(|s| s.start < visible.end);
        lo..hi.max(lo)
    }

    /// The worst-severity diagnostic whose rows straddle `line`.
    ///
    /// The answer for one line is kept, so a frame whose cursor has not changed
    /// line reads it back rather than searching again.
    ///
    /// Ties go to the earliest span, matching a scan of the whole slice: the
    /// bound below preserves the order the spans are sorted in.
    pub(crate) fn cursor_line_diagnostic(&mut self, line: u32) -> Option<ResolvedDiag> {
        if let Some((cached, found)) = self.cursor_line_diag
            && cached == line
        {
            return found;
        }

        let found = self.spans[self.straddling_line(line)]
            .iter()
            .filter(|s| s.start_line <= line && line <= s.end_line)
            .min_by_key(|s| severity_rank(s.severity))
            .copied();

        self.cursor_line_diag = Some((line, found));
        found
    }

    /// The index range of [`Self::spans`] with rows that straddle `line`.
    ///
    /// Spans outside it end above the line or start below it, so a caller still
    /// filters the ones inside for real containment.
    fn straddling_line(&self, line: u32) -> Range<usize> {
        let lo = self.prefix_max_end_line.partition_point(|&e| e < line);
        let hi = self.spans.partition_point(|s| s.start_line <= line);
        lo..hi.max(lo)
    }
}

/// Resolve every diagnostic for `path` to byte offsets, sorted by start.
///
/// Each range is converted through the offset encoding its publishing server
/// negotiated (a server absent from `encodings` falls back to UTF-16), so a
/// utf-16 server's diagnostic on a multibyte line lands on the right byte. The
/// index into `set.get(path)` is retained so callers can recover the source
/// diagnostic.
pub(crate) fn resolve_diagnostic_spans(
    set: &crate::diagnostics::DiagnosticSet,
    path: &Path,
    snapshot: &crate::multi_buffer::MultiBufferSnapshot,
) -> Vec<ResolvedDiag> {
    let diagnostics = set.get(path);
    let published = set.spans(path);

    // Every endpoint in one walk of the fragment tree rather than a root descent
    // apiece, then their lines in one walk of the rope. A diagnostic with no
    // anchor pair contributes none, so the running slot below is what pairs the
    // results back up with the diagnostics that asked for them.
    let anchored: Vec<bool> = (0..diagnostics.len())
        .map(|index| anchors_in(published.get(index), snapshot).is_some())
        .collect();
    let endpoints: Vec<Anchor> = (0..diagnostics.len())
        .filter_map(|index| anchors_in(published.get(index), snapshot))
        .flat_map(|(start, end)| [start, end])
        .collect();
    let offsets = snapshot.resolve_anchors_batch(&endpoints);
    let points = snapshot.rope().offsets_to_points_batch(&offsets);

    let mut slot = 0usize;
    let mut spans: Vec<ResolvedDiag> = diagnostics
        .iter()
        .enumerate()
        .map(|(index, diag)| {
            let (start, end, start_line, end_line) = if anchored[index] {
                let resolved = (
                    offsets[slot],
                    offsets[slot + 1],
                    points[slot].row,
                    points[slot + 1].row,
                );
                slot += 2;
                resolved
            } else {
                (0, 0, 0, 0)
            };
            ResolvedDiag {
                start,
                end,
                severity: diag.severity.unwrap_or(DiagnosticSeverity::ERROR),
                unnecessary: is_unnecessary(diag),
                start_line,
                end_line,
                index,
            }
        })
        .collect();
    spans.sort_by_key(|s| s.start);
    spans
}

/// `span`'s endpoints when they anchor into the buffer `snapshot` is for.
///
/// A span published against a different buffer resolves to a meaningless offset
/// rather than an error, since the fragment tree it names is not the one being
/// asked, so the buffer has to be checked before the anchors are used.
fn anchors_in(
    span: Option<&crate::diagnostics::PublishedSpan>,
    snapshot: &crate::multi_buffer::MultiBufferSnapshot,
) -> Option<(Anchor, Anchor)> {
    let (start, end) = span?.anchors?;
    (start.buffer_id == Some(snapshot.buffer_id())).then_some((start, end))
}

/// Rebuild `editor.diagnostic_span_cache` when the diagnostic set or buffer
/// version has moved since it was last resolved.
///
/// The paint is not the only caller. Mouse motion warms the same cache to
/// answer what the pointer is over, so a sweep across a diagnostic-heavy
/// buffer resolves the spans once rather than once per motion event.
pub(crate) fn build_diagnostic_span_cache(
    editor: &mut EditorState,
    set: &crate::diagnostics::DiagnosticSet,
    path: &Path,
    snapshot: &crate::multi_buffer::MultiBufferSnapshot,
) {
    let buffer_version = snapshot.version();
    let set_version = set.version();
    let stale = match &editor.diagnostic_span_cache {
        Some(cache) => cache.set_version != set_version || cache.buffer_version != buffer_version,
        None => true,
    };
    if stale {
        let spans = resolve_diagnostic_spans(set, path, snapshot);
        editor.diagnostic_span_cache = Some(DiagnosticSpanCache {
            set_version,
            buffer_version,
            prefix_max_end: prefix_max_ends(&spans),
            prefix_max_end_line: prefix_max_end_lines(&spans),
            cursor_line_diag: None,
            spans,
        });
    }
}

/// The running maximum of `spans`' ends, for [`DiagnosticSpanCache::prefix_max_end`].
fn prefix_max_ends(spans: &[ResolvedDiag]) -> Vec<usize> {
    let mut max_end = 0;
    spans
        .iter()
        .map(|span| {
            max_end = max_end.max(span.end);
            max_end
        })
        .collect()
}

/// The running maximum of `spans`' end rows, for
/// [`DiagnosticSpanCache::prefix_max_end_line`].
fn prefix_max_end_lines(spans: &[ResolvedDiag]) -> Vec<u32> {
    let mut max_end = 0;
    spans
        .iter()
        .map(|span| {
            max_end = max_end.max(span.end_line);
            max_end
        })
        .collect()
}

pub(crate) fn severity_rank(sev: DiagnosticSeverity) -> u8 {
    match sev {
        DiagnosticSeverity::ERROR => 0,
        DiagnosticSeverity::WARNING => 1,
        DiagnosticSeverity::INFORMATION => 2,
        DiagnosticSeverity::HINT => 3,
        _ => 0,
    }
}

/// Whether a diagnostic carries the `Unnecessary` tag, marking dead or
/// inactive code (e.g. a `#[cfg]`-excluded region) that renders muted rather
/// than underlined.
fn is_unnecessary(diag: &lsp_types::Diagnostic) -> bool {
    diag.tags
        .as_ref()
        .is_some_and(|tags| tags.contains(&DiagnosticTag::UNNECESSARY))
}

/// Index into `set.get(path)` of the highest-severity diagnostic whose byte
/// range contains `offset`, or `None` when none do.
///
/// `spans` is [`resolve_diagnostic_spans`] output, sorted by start. A
/// `partition_point` bounds the scan to spans starting at or before `offset`.
/// The worst severity wins a tie, matching the gutter and the EOL message.
pub(crate) fn diagnostic_at_offset(spans: &[ResolvedDiag], offset: usize) -> Option<usize> {
    let hi = spans.partition_point(|s| s.start <= offset);
    spans[..hi]
        .iter()
        .filter(|s| s.start < s.end && offset < s.end)
        .min_by_key(|s| severity_rank(s.severity))
        .map(|s| s.index)
}

#[cfg(test)]
mod tests {
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
    use std::path::PathBuf;
    use stoat_text::Bias;

    /// Each resolved diagnostic as `(index, start, end, start_line)`.
    fn resolved(
        set: &crate::diagnostics::DiagnosticSet,
        path: &std::path::Path,
        snapshot: &crate::multi_buffer::MultiBufferSnapshot,
    ) -> Vec<(usize, usize, usize, u32)> {
        super::resolve_diagnostic_spans(set, path, snapshot)
            .iter()
            .map(|s| (s.index, s.start, s.end, s.start_line))
            .collect()
    }

    /// A diagnostic nothing could anchor has no position of its own, and must
    /// not take the offsets belonging to the ones beside it.
    #[test]
    fn a_diagnostic_without_an_anchored_span_resolves_to_zero() {
        let path = PathBuf::from("/a");
        let snapshot = snapshot_over("alpha\nbravo\n");
        let mut set = crate::diagnostics::DiagnosticSet::new();
        set.replace_from_server(
            path.clone(),
            "lsp".into(),
            vec![
                span_diag(0, 5, DiagnosticSeverity::ERROR),
                span_diag(0, 5, DiagnosticSeverity::WARNING),
            ],
            vec![
                crate::diagnostics::PublishedSpan::unresolved(),
                anchored_span(&snapshot, 6..11),
            ],
        );

        assert_eq!(
            resolved(&set, &path, &snapshot),
            [(0, 0, 0, 0), (1, 6, 11, 1)],
            "the unanchored one sits at zero and the anchored one keeps its span",
        );
    }

    /// An anchor names a position in one buffer's fragment tree. Resolving it
    /// against another answers with an offset rather than an error, so a span
    /// published elsewhere has to read as unanchored instead.
    #[test]
    fn a_span_anchored_in_another_buffer_resolves_to_zero() {
        use crate::{buffer::TextBuffer, multi_buffer::MultiBuffer};
        use std::sync::{Arc, RwLock};

        let path = PathBuf::from("/a");
        let other_id = stoat_text::BufferId::new(7);
        let other = MultiBuffer::singleton(
            other_id,
            Arc::new(RwLock::new(TextBuffer::with_text(other_id, "elsewhere\n"))),
        )
        .snapshot();

        let snapshot = snapshot_over("alpha\nbravo\n");
        let mut set = crate::diagnostics::DiagnosticSet::new();
        set.replace_from_server(
            path.clone(),
            "lsp".into(),
            vec![span_diag(0, 5, DiagnosticSeverity::ERROR)],
            vec![anchored_span(&other, 2..6)],
        );

        assert_eq!(resolved(&set, &path, &snapshot), [(0, 0, 0, 0)]);
    }

    fn span_diag(start: u32, end: u32, sev: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: start,
                },
                end: Position {
                    line: 0,
                    character: end,
                },
            },
            severity: Some(sev),
            message: String::new(),
            ..Default::default()
        }
    }

    /// A singleton multi-buffer over `content`, for tests that need a snapshot
    /// to resolve diagnostics against.
    fn snapshot_over(content: &str) -> crate::multi_buffer::MultiBufferSnapshot {
        use crate::{buffer::TextBuffer, multi_buffer::MultiBuffer};
        use std::sync::{Arc, RwLock};
        let id = stoat_text::BufferId::new(0);
        let buffer = TextBuffer::with_text(id, content);
        MultiBuffer::singleton(id, Arc::new(RwLock::new(buffer))).snapshot()
    }

    /// A span anchored over `range` in `snapshot`, the way a publish against
    /// that text would have anchored it.
    fn anchored_span(
        snapshot: &crate::multi_buffer::MultiBufferSnapshot,
        range: std::ops::Range<usize>,
    ) -> crate::diagnostics::PublishedSpan {
        crate::diagnostics::PublishedSpan {
            anchors: Some((
                snapshot.anchors_at_batch(&[range.start], Bias::Right)[0],
                snapshot.anchors_at_batch(&[range.end], Bias::Left)[0],
            )),
        }
    }

    #[test]
    fn diagnostic_at_offset_finds_worst_containing_span() {
        let path = PathBuf::from("/a");
        let snapshot = snapshot_over("let x = 1;\n");
        let mut set = crate::diagnostics::DiagnosticSet::new();
        // A warning over just `x` [4,5) and an error over `x = 1` [4,9).
        set.replace_from_server(
            path.clone(),
            "lsp".into(),
            vec![
                span_diag(4, 5, DiagnosticSeverity::WARNING),
                span_diag(4, 9, DiagnosticSeverity::ERROR),
            ],
            vec![
                anchored_span(&snapshot, 4..5),
                anchored_span(&snapshot, 4..9),
            ],
        );

        let spans = super::resolve_diagnostic_spans(&set, &path, &snapshot);
        // Offset 4 is in both, so the worse severity (the error) wins.
        assert_eq!(super::diagnostic_at_offset(&spans, 4), Some(1));
        // Offset 7 is inside only the error span.
        assert_eq!(super::diagnostic_at_offset(&spans, 7), Some(1));
        // Offset 0 is outside both.
        assert_eq!(super::diagnostic_at_offset(&spans, 0), None);
    }

    /// A cache over spans at the given byte ranges, ordered as
    /// [`super::resolve_diagnostic_spans`] leaves them.
    fn span_cache(ranges: &[(usize, usize)]) -> super::DiagnosticSpanCache {
        let spans: Vec<super::ResolvedDiag> = ranges
            .iter()
            .enumerate()
            .map(|(index, &(start, end))| super::ResolvedDiag {
                start,
                end,
                severity: DiagnosticSeverity::WARNING,
                unnecessary: false,
                start_line: 0,
                end_line: 0,
                index,
            })
            .collect();

        super::DiagnosticSpanCache {
            set_version: 0,
            buffer_version: 0,
            prefix_max_end: super::prefix_max_ends(&spans),
            prefix_max_end_line: super::prefix_max_end_lines(&spans),
            cursor_line_diag: None,
            spans,
        }
    }

    /// Spans are ordered by start, so a long one opening above the viewport sits
    /// among short ones that close before it reaches them. Bounding the walk by
    /// the running maximum of the ends is what keeps that span while still
    /// skipping its neighbours.
    #[test]
    fn the_overlap_bound_keeps_a_span_reaching_in_from_above() {
        let cache = span_cache(&[
            (0, 5),
            (10, 400),
            (20, 25),
            (30, 35),
            (200, 205),
            (500, 505),
        ]);

        assert_eq!(
            cache.overlapping(100..300),
            1..5,
            "the walk opens at the long span and closes before the one below",
        );
        assert_eq!(
            cache.overlapping(600..700),
            6..6,
            "a viewport past every span walks nothing",
        );
    }

    /// A zero-width span sits at an offset it neither starts before nor ends
    /// after, which leaves the two bounds crossed. The bound has to come back in
    /// order anyway, since the caller indexes the spans with it.
    #[test]
    fn the_overlap_bound_survives_a_zero_width_span_at_the_viewport_start() {
        let cache = span_cache(&[(5, 5)]);

        assert!(cache.spans[cache.overlapping(5..5)].is_empty());
    }

    /// A cache over spans at the given `(start_line, end_line, severity)`, laid
    /// out one line apart so the byte order matches the row order.
    fn line_span_cache(rows: &[(u32, u32, DiagnosticSeverity)]) -> super::DiagnosticSpanCache {
        let spans: Vec<super::ResolvedDiag> = rows
            .iter()
            .enumerate()
            .map(
                |(index, &(start_line, end_line, severity))| super::ResolvedDiag {
                    start: start_line as usize * 10,
                    end: end_line as usize * 10,
                    severity,
                    unnecessary: false,
                    start_line,
                    end_line,
                    index,
                },
            )
            .collect();

        super::DiagnosticSpanCache {
            set_version: 0,
            buffer_version: 0,
            prefix_max_end: super::prefix_max_ends(&spans),
            prefix_max_end_line: super::prefix_max_end_lines(&spans),
            cursor_line_diag: None,
            spans,
        }
    }

    /// A diagnostic reaching the cursor's line from above still wins the
    /// readout, and a bound in rows is what keeps it. The end offset of a span
    /// ending at column 0 is the byte the line starts at, so a bound in offsets
    /// reads it as settled and drops the span the row filter accepts.
    #[test]
    fn the_line_bound_keeps_a_span_ending_at_the_line_start() {
        let mut cache = line_span_cache(&[
            (0, 3, DiagnosticSeverity::WARNING),
            (5, 5, DiagnosticSeverity::ERROR),
        ]);

        assert_eq!(
            cache.cursor_line_diagnostic(3).map(|d| d.index),
            Some(0),
            "the span reaching line 3 from line 0 wins it",
        );
        assert_eq!(
            cache.cursor_line_diagnostic(4).map(|d| d.index),
            None,
            "and the line below it is inside nothing",
        );
    }

    /// The worst severity wins a line, and ties go to the earliest span, which
    /// is what a scan of the whole slice does. The bound preserves the order the
    /// spans are sorted in, so narrowing it leaves both answers alone.
    #[test]
    fn the_worst_severity_wins_the_cursor_line() {
        let mut cache = line_span_cache(&[
            (2, 2, DiagnosticSeverity::WARNING),
            (2, 2, DiagnosticSeverity::ERROR),
            (2, 2, DiagnosticSeverity::ERROR),
        ]);

        assert_eq!(
            cache.cursor_line_diagnostic(2).map(|d| d.index),
            Some(1),
            "the first of the two errors wins over the warning",
        );
    }

    /// The readout runs on every frame the cursor sits inside a diagnostic, and
    /// only a cursor that changes line changes the answer. Clearing the spans
    /// under the cache is what makes the reuse visible. A fresh search over the
    /// emptied slice finds nothing.
    #[test]
    fn a_cursor_that_stays_on_its_line_answers_from_the_memo() {
        let mut cache = line_span_cache(&[(1, 1, DiagnosticSeverity::ERROR)]);

        assert_eq!(cache.cursor_line_diagnostic(1).map(|d| d.index), Some(0));

        // Emptied together, since the bound indexes the spans through the prefix
        // maximums and a real cache never holds one without the others.
        cache.spans.clear();
        cache.prefix_max_end.clear();
        cache.prefix_max_end_line.clear();

        assert_eq!(
            cache.cursor_line_diagnostic(1).map(|d| d.index),
            Some(0),
            "the same line answers from the memo rather than searching again",
        );
        assert_eq!(
            cache.cursor_line_diagnostic(2).map(|d| d.index),
            None,
            "and a moved cursor searches, over the spans that are there now",
        );
    }

    /// Anchoring at publish is what makes a mark follow its text. The offsets a
    /// server named are in the coordinates of text every later edit moves, so a
    /// resolution reading them back would drift off what it marked.
    #[test]
    fn an_edit_above_a_diagnostic_carries_it_down() {
        use crate::{buffer::TextBuffer, multi_buffer::MultiBuffer};
        use std::sync::{Arc, RwLock};

        let path = PathBuf::from("/a");
        let id = stoat_text::BufferId::new(0);
        let buffer = Arc::new(RwLock::new(TextBuffer::with_text(id, "alpha\nbravo\n")));
        let multi = MultiBuffer::singleton(id, buffer.clone());

        // `bravo` is [6, 11), on the second line.
        let published = multi.snapshot();
        let mut set = crate::diagnostics::DiagnosticSet::new();
        set.replace_from_server(
            path.clone(),
            "lsp".into(),
            vec![span_diag(0, 5, DiagnosticSeverity::ERROR)],
            vec![anchored_span(&published, 6..11)],
        );

        assert_eq!(
            resolved(&set, &path, &published),
            [(0, 6, 11, 1)],
            "over the word it was published on",
        );

        // Nine bytes and a line inserted ahead of it, with no republish behind.
        buffer.write().expect("poisoned").edit(0..0, "inserted\n");

        assert_eq!(
            resolved(&set, &path, &multi.snapshot()),
            [(0, 15, 20, 2)],
            "and still over that word, a line further down",
        );
    }
}
