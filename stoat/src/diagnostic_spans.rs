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
use stoat_text::{Anchor, Bias};

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

/// A diagnostic still in the anchors [`crate::diagnostics`] published it with.
///
/// Held instead of byte offsets so an edit does not stale the cache. Anchors
/// follow their text, so the same pair answers for every buffer version, and a
/// query resolves the few it actually returns.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AnchoredDiag {
    pub(crate) start: Anchor,
    pub(crate) end: Anchor,
    pub(crate) severity: DiagnosticSeverity,
    pub(crate) unnecessary: bool,
    /// The position in `set.get(path)`, so a caller recovers the diagnostic's
    /// message and tags after locating it.
    pub(crate) index: usize,
}

/// Per-editor cache of the diagnostics for one path, in publish anchors.
///
/// Keyed on the path's own diagnostic version alone. A buffer edit leaves it
/// standing, because anchors already describe where the text went, so typing in
/// a file full of diagnostics costs nothing here. Transient render state, not
/// persisted.
pub(crate) struct DiagnosticSpanCache {
    set_version: u64,
    /// Sorted by start offset as resolved when the cache was built.
    ///
    /// One consistent resolve settles the order, and anchors move monotonically
    /// from there, so no later edit reorders them. That is what lets every
    /// query binary-search without resolving the whole list again.
    pub(crate) spans: Vec<AnchoredDiag>,
    /// Running argmax of [`Self::spans`]' ends, one entry per span.
    ///
    /// The spans are sorted by start, so their ends are not, and a viewport's
    /// lower bound is not searchable directly. Entry `i` names the span
    /// with the greatest end among `0..=i`, which is enough to prove every span
    /// up to `i` ends at or before an offset. An index rather than an offset,
    /// since the end it stands for has to be resolved against the snapshot the
    /// query asks about rather than the one that built this.
    prefix_max_end: Vec<u32>,
    /// The cursor line the readout last answered for, and the diagnostic it
    /// found there.
    ///
    /// The readout runs on every frame the cursor sits inside a diagnostic, and
    /// the answer only moves when the cursor changes line. Keyed on the buffer
    /// version too, unlike the spans, because a line number means something
    /// different after an edit above it.
    cursor_line_diag: Option<(u32, u64, Option<ResolvedDiag>)>,
}

impl DiagnosticSpanCache {
    /// The index range of [`Self::spans`] that bounds every overlap with
    /// `byte_range`.
    ///
    /// Resolves O(log n) probe anchors rather than every span.
    ///
    /// Spans in the range still need a per-span end check. Some end at or
    /// before `byte_range.start` and ride along under the max end of a span
    /// that encloses the range from earlier.
    pub(crate) fn overlapping(
        &self,
        byte_range: &Range<usize>,
        snapshot: &crate::multi_buffer::MultiBufferSnapshot,
    ) -> Range<usize> {
        let resolve = |anchor: &Anchor| snapshot.resolve_anchor(anchor);

        let hi = {
            let (mut left, mut right) = (0, self.spans.len());
            while left < right {
                let mid = left + (right - left) / 2;
                if resolve(&self.spans[mid].start) < byte_range.end {
                    left = mid + 1;
                } else {
                    right = mid;
                }
            }
            left
        };

        let (mut left, mut right) = (0, hi);
        while left < right {
            let mid = left + (right - left) / 2;
            let max_end = &self.spans[self.prefix_max_end[mid] as usize].end;
            if resolve(max_end) > byte_range.start {
                right = mid;
            } else {
                left = mid + 1;
            }
        }
        left..hi
    }

    /// The spans the bounds keep for `byte_range`, resolved to offsets.
    ///
    /// One batch resolve over the spans the bounds kept, so the cost follows
    /// what the viewport covers rather than what the file holds.
    pub(crate) fn resolve_overlapping(
        &self,
        byte_range: &Range<usize>,
        snapshot: &crate::multi_buffer::MultiBufferSnapshot,
    ) -> Vec<ResolvedDiag> {
        let bounds = self.overlapping(byte_range, snapshot);
        resolve_span_range(&self.spans[bounds], snapshot)
    }

    /// The worst-severity diagnostic whose rows straddle `line`.
    ///
    /// The answer for one line is kept, so a frame whose cursor has not changed
    /// line reads it back rather than searching again.
    ///
    /// Ties go to the earliest span, matching a scan of the whole slice, since
    /// the bounds below preserve the order the spans are sorted in.
    pub(crate) fn cursor_line_diagnostic(
        &mut self,
        line: u32,
        snapshot: &crate::multi_buffer::MultiBufferSnapshot,
    ) -> Option<ResolvedDiag> {
        let buffer_version = snapshot.version();
        if let Some((cached_line, cached_version, found)) = self.cursor_line_diag
            && cached_line == line
            && cached_version == buffer_version
        {
            return found;
        }

        // A diagnostic ending at column 0 of a line still counts as reaching
        // that line, so the probe covers the line's own bytes and the byte
        // before it.
        let rope = snapshot.rope();
        let line_start = rope.point_to_offset(stoat_text::Point::new(line, 0));
        let probe = line_start.saturating_sub(1)..line_start + rope.line_len(line) as usize + 1;

        let found = self
            .resolve_overlapping(&probe, snapshot)
            .into_iter()
            .filter(|s| s.start_line <= line && line <= s.end_line)
            .min_by_key(|s| severity_rank(s.severity));

        self.cursor_line_diag = Some((line, buffer_version, found));
        found
    }
}

/// Every diagnostic for `path` in its publish anchors, sorted by start.
///
/// One consistent resolve settles the order, which the anchors then carry
/// through every later edit unchanged, so this runs once per publish rather
/// than once per keystroke.
///
/// A diagnostic whose anchors are missing, or belong to another buffer, takes
/// an anchor at the buffer start, matching where
/// [`resolve_diagnostic_spans`] puts it. It has no position of its own, and one
/// at the start is what every downstream filter already expects.
fn anchored_diagnostics(
    set: &crate::diagnostics::DiagnosticSet,
    path: &Path,
    snapshot: &crate::multi_buffer::MultiBufferSnapshot,
) -> Vec<AnchoredDiag> {
    let diagnostics = set.get(path);
    let published = set.spans(path);
    let unanchored = snapshot.anchor_at(0, Bias::Right);

    let spans: Vec<AnchoredDiag> = diagnostics
        .iter()
        .enumerate()
        .map(|(index, diag)| {
            let (start, end) =
                anchors_in(published.get(index), snapshot).unwrap_or((unanchored, unanchored));
            AnchoredDiag {
                start,
                end,
                severity: diag.severity.unwrap_or(DiagnosticSeverity::ERROR),
                unnecessary: is_unnecessary(diag),
                index,
            }
        })
        .collect();

    let starts = snapshot
        .resolve_anchors_batch(&spans.iter().map(|span| span.start).collect::<Vec<Anchor>>());
    // Sorted by the start each span resolved to, which the parallel Vec above
    // holds by index, so a sort key never re-resolves.
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|&i| (starts[i], i));

    order.into_iter().map(|i| spans[i]).collect()
}

/// The endpoints of `spans` resolved against `snapshot`.
///
/// Every endpoint in one walk of the fragment tree rather than a root descent
/// apiece, then their lines in one walk of the rope.
fn resolve_span_range(
    spans: &[AnchoredDiag],
    snapshot: &crate::multi_buffer::MultiBufferSnapshot,
) -> Vec<ResolvedDiag> {
    let endpoints: Vec<Anchor> = spans
        .iter()
        .flat_map(|span| [span.start, span.end])
        .collect();
    let offsets = snapshot.resolve_anchors_batch(&endpoints);
    let points = snapshot.rope().offsets_to_points_batch(&offsets);

    spans
        .iter()
        .enumerate()
        .map(|(slot, span)| ResolvedDiag {
            start: offsets[slot * 2],
            end: offsets[slot * 2 + 1],
            severity: span.severity,
            unnecessary: span.unnecessary,
            start_line: points[slot * 2].row,
            end_line: points[slot * 2 + 1].row,
            index: span.index,
        })
        .collect()
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
    let set_version = set.version_for(path);
    let stale = match &editor.diagnostic_span_cache {
        Some(cache) => cache.set_version != set_version,
        None => true,
    };
    if stale {
        let spans = anchored_diagnostics(set, path, snapshot);
        editor.diagnostic_span_cache = Some(DiagnosticSpanCache {
            set_version,
            prefix_max_end: prefix_max_end_indices(&spans, snapshot),
            cursor_line_diag: None,
            spans,
        });
    }
}

/// The running argmax of `spans`' ends, for
/// [`DiagnosticSpanCache::prefix_max_end`].
///
/// Entry `i` is the index of the greatest end among `0..=i`. Ties keep the
/// earlier index, which is not observable: both name a span with the same end,
/// so a search reads the same bound either way.
///
/// Resolved once here against the snapshot that builds the cache. Anchors move
/// monotonically, so the span holding the greatest end holds it for every later
/// version too, and only its offset moves.
fn prefix_max_end_indices(
    spans: &[AnchoredDiag],
    snapshot: &crate::multi_buffer::MultiBufferSnapshot,
) -> Vec<u32> {
    let ends =
        snapshot.resolve_anchors_batch(&spans.iter().map(|span| span.end).collect::<Vec<Anchor>>());

    let mut indices = Vec::with_capacity(ends.len());
    let mut max_end = 0;
    let mut max_idx = 0;

    for (i, &end) in ends.iter().enumerate() {
        if i == 0 || end > max_end {
            max_end = end;
            max_idx = i as u32;
        }
        indices.push(max_idx);
    }

    indices
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

/// The highest-severity diagnostic whose byte range contains `offset`, or
/// `None` when none do.
///
/// Resolves only the spans the cache's bounds keep for the byte `offset` sits
/// on, rather than every span before it. The worst severity wins a tie,
/// matching the gutter and the EOL message.
///
/// The resolved span comes back with the answer, so a caller that needs the
/// diagnostic's position reads it here rather than searching for it again.
pub(crate) fn diagnostic_at_offset(
    cache: &DiagnosticSpanCache,
    offset: usize,
    snapshot: &crate::multi_buffer::MultiBufferSnapshot,
) -> Option<ResolvedDiag> {
    cache
        .resolve_overlapping(&(offset..offset + 1), snapshot)
        .into_iter()
        .filter(|s| s.start < s.end && s.start <= offset && offset < s.end)
        .min_by_key(|s| severity_rank(s.severity))
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
        let spans = super::anchored_diagnostics(set, path, snapshot);
        super::resolve_span_range(&spans, snapshot)
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
        let other = MultiBuffer::singleton(Arc::new(RwLock::new(TextBuffer::with_text(
            other_id,
            "elsewhere\n",
        ))))
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
        MultiBuffer::singleton(Arc::new(RwLock::new(buffer))).snapshot()
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

        let cache = cache_over(
            &snapshot,
            super::anchored_diagnostics(&set, &path, &snapshot),
        );
        let at = |offset| super::diagnostic_at_offset(&cache, offset, &snapshot).map(|d| d.index);

        // Offset 4 is in both, so the worse severity (the error) wins.
        assert_eq!(at(4), Some(1));
        // Offset 7 is inside only the error span.
        assert_eq!(at(7), Some(1));
        // Offset 0 is outside both.
        assert_eq!(at(0), None);
    }

    /// A cache over spans at the given byte ranges, with a buffer long enough
    /// for each to name a real position.
    fn span_cache(
        ranges: &[(usize, usize)],
    ) -> (
        super::DiagnosticSpanCache,
        crate::multi_buffer::MultiBufferSnapshot,
    ) {
        let len = ranges.iter().map(|&(_, end)| end).max().unwrap_or(0);
        let snapshot = snapshot_over(&"a".repeat(len + 1));
        let spans = anchored(&snapshot, ranges, DiagnosticSeverity::WARNING);
        (cache_over(&snapshot, spans), snapshot)
    }

    /// Anchor each byte range against `snapshot`, in the order given.
    fn anchored(
        snapshot: &crate::multi_buffer::MultiBufferSnapshot,
        ranges: &[(usize, usize)],
        severity: DiagnosticSeverity,
    ) -> Vec<super::AnchoredDiag> {
        ranges
            .iter()
            .enumerate()
            .map(|(index, &(start, end))| super::AnchoredDiag {
                start: snapshot.anchor_at(start, Bias::Right),
                end: snapshot.anchor_at(end, Bias::Left),
                severity,
                unnecessary: false,
                index,
            })
            .collect()
    }

    fn cache_over(
        snapshot: &crate::multi_buffer::MultiBufferSnapshot,
        spans: Vec<super::AnchoredDiag>,
    ) -> super::DiagnosticSpanCache {
        super::DiagnosticSpanCache {
            set_version: 0,
            prefix_max_end: super::prefix_max_end_indices(&spans, snapshot),
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
        let (cache, snapshot) = span_cache(&[
            (0, 5),
            (10, 400),
            (20, 25),
            (30, 35),
            (200, 205),
            (500, 505),
        ]);

        assert_eq!(
            cache.overlapping(&(100..300), &snapshot),
            1..5,
            "the walk opens at the long span and closes before the one below",
        );
        assert_eq!(
            cache.overlapping(&(600..700), &snapshot),
            6..6,
            "a viewport past every span walks nothing",
        );
    }

    /// A zero-width span sits at an offset it neither starts before nor ends
    /// after, which leaves the two bounds crossed. The bound has to come back in
    /// order anyway, since the caller indexes the spans with it.
    #[test]
    fn the_overlap_bound_survives_a_zero_width_span_at_the_viewport_start() {
        let (cache, snapshot) = span_cache(&[(5, 5)]);

        assert!(cache.spans[cache.overlapping(&(5..5), &snapshot)].is_empty());
    }

    /// A cache over spans at the given `(start_line, end_line, severity)`, over
    /// a buffer of ten-byte lines so a row's start offset is `row * 10`.
    ///
    /// Each span ends at column 0 of its end row, which is the case a bound in
    /// offsets alone gets wrong.
    fn line_span_cache(
        rows: &[(u32, u32, DiagnosticSeverity)],
    ) -> (
        super::DiagnosticSpanCache,
        crate::multi_buffer::MultiBufferSnapshot,
    ) {
        let last = rows.iter().map(|&(_, end, _)| end).max().unwrap_or(0);
        let text: String = (0..=last + 1).map(|_| "aaaaaaaaa\n").collect();
        let snapshot = snapshot_over(&text);

        let spans: Vec<super::AnchoredDiag> = rows
            .iter()
            .enumerate()
            .map(
                |(index, &(start_line, end_line, severity))| super::AnchoredDiag {
                    start: snapshot.anchor_at(start_line as usize * 10, Bias::Right),
                    end: snapshot.anchor_at(end_line as usize * 10, Bias::Left),
                    severity,
                    unnecessary: false,
                    index,
                },
            )
            .collect();

        (cache_over(&snapshot, spans), snapshot)
    }

    /// A diagnostic reaching the cursor's line from above still wins the
    /// readout, and a bound in rows is what keeps it. The end offset of a span
    /// ending at column 0 is the byte the line starts at, so a bound in offsets
    /// reads it as settled and drops the span the row filter accepts.
    #[test]
    fn the_line_bound_keeps_a_span_ending_at_the_line_start() {
        let (mut cache, snapshot) = line_span_cache(&[
            (0, 3, DiagnosticSeverity::WARNING),
            (5, 5, DiagnosticSeverity::ERROR),
        ]);

        assert_eq!(
            cache.cursor_line_diagnostic(3, &snapshot).map(|d| d.index),
            Some(0),
            "the span reaching line 3 from line 0 wins it",
        );
        assert_eq!(
            cache.cursor_line_diagnostic(4, &snapshot).map(|d| d.index),
            None,
            "and the line below it is inside nothing",
        );
    }

    /// The worst severity wins a line, and ties go to the earliest span, which
    /// is what a scan of the whole slice does. The bound preserves the order the
    /// spans are sorted in, so narrowing it leaves both answers alone.
    #[test]
    fn the_worst_severity_wins_the_cursor_line() {
        let (mut cache, snapshot) = line_span_cache(&[
            (2, 2, DiagnosticSeverity::WARNING),
            (2, 2, DiagnosticSeverity::ERROR),
            (2, 2, DiagnosticSeverity::ERROR),
        ]);

        assert_eq!(
            cache.cursor_line_diagnostic(2, &snapshot).map(|d| d.index),
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
        let (mut cache, snapshot) = line_span_cache(&[(1, 1, DiagnosticSeverity::ERROR)]);

        assert_eq!(
            cache.cursor_line_diagnostic(1, &snapshot).map(|d| d.index),
            Some(0)
        );

        // Emptied together, since the bound indexes the spans through the prefix
        // maximums and a real cache never holds one without the other.
        cache.spans.clear();
        cache.prefix_max_end.clear();

        assert_eq!(
            cache.cursor_line_diagnostic(1, &snapshot).map(|d| d.index),
            Some(0),
            "the same line answers from the memo rather than searching again",
        );
        assert_eq!(
            cache.cursor_line_diagnostic(2, &snapshot).map(|d| d.index),
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
        let multi = MultiBuffer::singleton(buffer.clone());

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
