//! Building a buffer's diff against git, and the state that keeps it fresh.
//!
//! A diff map is what the gutter marks, the diff view, and the change motions
//! all read. Producing one takes the repo mutex, decompresses the HEAD and index
//! blobs, and diffs the whole file, so none of it belongs on the run loop and
//! none of it should happen twice for the same text. What lives here is that
//! pipeline: the compute functions the blocking jobs run, the two memos that
//! keep a keystroke from re-reading blobs or re-parsing a base text, and the
//! per-buffer bookkeeping that decides when a recompute is owed.
//!
//! [`Workspace`](super::Workspace) keeps the driving loop, since scheduling a
//! diff needs the buffer registry and the pane layout as well as this state.

use crate::{
    buffer::BufferId,
    buffer_registry,
    code_index::build::file_id,
    diff::{self, ReviewFileInput},
    diff_cache::ContentHash,
    diff_map::{changes_to_hunks, line_starts, BaseHighlights, DiffMap},
    display_map::syntax_theme::SyntaxStyles,
    host::{FsHost, GitHost},
};
use codegraph::FileId;
use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use stoat_language::{
    extract_highlights, parse, structural_diff, HighlightSpan, Language, LanguageRegistry,
};
use stoat_scheduler::Task;

/// How long a buffer must hold one version before its diff is recomputed.
///
/// A diff walks the whole file on a blocking thread and the next keystroke
/// invalidates it, so a typing burst is worth one diff at the end rather than
/// one per edit. Short enough that a reader pausing mid-thought still sees the
/// gutter catch up.
pub(crate) const DIFF_SETTLE: Duration = Duration::from_millis(250);

/// Per-file diff memo, keyed by graph [`FileId`], holding the base and buffer
/// content hashes the ranges were measured from alongside the ranges.
///
/// Both hashes still matching is what lets a scan reuse an entry rather than
/// diff the file again.
pub(crate) type ChangedRangesMemo = HashMap<FileId, (ContentHash, ContentHash, Vec<Range<usize>>)>;

/// A working-tree diff scan, ready to install on the workspace it ran for.
///
/// [`scan_changed_ranges`] produces one off the run loop, so what it learned has
/// to travel back as data rather than as a mutation.
#[derive(Default)]
pub(crate) struct ChangedRangesScan {
    /// The changed byte ranges per file, which becomes
    /// [`Workspace::changed_ranges`] whole.
    pub(super) ranges: HashMap<FileId, Vec<Range<usize>>>,
    /// The files the scan had to diff, with the hashes it diffed them at. The
    /// memo takes these on, so a scan finding the same texts reuses them.
    pub(super) computed: Vec<(FileId, ContentHash, ContentHash, Vec<Range<usize>>)>,
}

pub(super) struct DiffJob {
    pub(super) target_version: u64,
    pub(super) task: Task<DiffJobOutput>,
}

pub(super) struct DiffJobOutput {
    pub(super) buffer_id: BufferId,
    /// The file the job diffed, so its base text can be filed under the same
    /// key the next job for it will look under.
    pub(super) path: PathBuf,
    pub(super) target_version: u64,
    pub(super) diff_map: Option<DiffMap>,
    /// The blobs the diff ran against, `None` when the repo or the file's HEAD
    /// content could not be read and there was nothing to diff.
    pub(super) base: Option<DiffBaseText>,
}

/// A file's HEAD and index blobs as git last reported them.
///
/// Neither can change without a write under `.git`, which is watched, so a
/// diff recomputed for a keystroke can reuse what the last one read rather than
/// taking the repo mutex and decompressing the same bytes again.
#[derive(Clone)]
pub(super) struct DiffBaseText {
    head: Arc<String>,
    index: Arc<String>,
    /// Fingerprints of the two blobs, taken once here.
    ///
    /// The index is usually a blob of its own that happens to hold HEAD's
    /// bytes, and every recompute asks whether the two agree so it can reuse
    /// one diff for both. Reading them to answer that costs a pass over the
    /// file per settle, where a blob only changes when something writes under
    /// `.git`.
    head_hash: [u8; 32],
    index_hash: [u8; 32],
}

/// Memoized diff-view base-text work, shared across the blocking jobs that
/// build diff maps.
///
/// Two layers, because they go stale on different inputs. Both grow without
/// bound, which is what an editor session's finite set of base texts, languages,
/// and themes makes acceptable.
#[derive(Default)]
pub(crate) struct BaseHighlightMemo {
    /// Tree-sitter highlight spans for a base text, keyed by its content hash
    /// and language name, so an unchanged base is parsed once across edits.
    /// Theme-independent.
    parses: HashMap<(ContentHash, String), Arc<Vec<HighlightSpan>>>,
    /// Those spans resolved to styles and split per base line. Keyed
    /// additionally by the [`SyntaxStyles`] generation, since the resolution is
    /// what the theme changes.
    buckets: HashMap<(ContentHash, String, u64), Arc<BaseHighlights>>,
}

pub(crate) type BaseHighlightCache = Arc<Mutex<BaseHighlightMemo>>;

/// Compute a buffer's HEAD-vs-worktree [`DiffMap`], or [`None`] when the file
/// is outside a repo or has no HEAD content to diff against.
///
/// Both `discover` and `head_content` do git and filesystem IO, so this must
/// run on a blocking thread. Uses the language-agnostic line diff, matching
/// [`changed_byte_ranges`].
#[allow(clippy::too_many_arguments)]
pub(super) fn compute_diff_map(
    git: &dyn GitHost,
    git_root: &Path,
    path: &Path,
    buffer_text: &str,
    language: Option<&Arc<Language>>,
    syntax_styles: &SyntaxStyles,
    base_cache: &BaseHighlightCache,
    cached_base: Option<DiffBaseText>,
) -> Option<(DiffMap, DiffBaseText)> {
    // Reading the blobs is what costs. It takes the repo mutex and then
    // decompresses bytes that a keystroke cannot have changed. The pair is
    // handed back either way so the caller can file it for the next one.
    let base = match cached_base {
        Some(base) => base,
        None => {
            let repo = git.discover(git_root)?;
            let head = Arc::new(repo.head_content(path)?);
            // A file with no index entry shares HEAD's handle rather than a
            // copy of its bytes, which the fingerprint below then reads off.
            let index = match repo.index_content(path) {
                Some(index) => Arc::new(index),
                None => head.clone(),
            };
            let head_hash = buffer_registry::fingerprint_bytes(&head);
            let index_hash = match Arc::ptr_eq(&head, &index) {
                true => head_hash,
                false => buffer_registry::fingerprint_bytes(&index),
            };
            DiffBaseText {
                head,
                index,
                head_hash,
                index_hash,
            }
        },
    };
    let base_text = &*base.head;
    let index_text = &*base.index;

    let result = structural_diff::diff(base_text, buffer_text);

    // Which buffer lines the index and the buffer disagree on, which is what
    // marks a hunk staged. With nothing staged the index holds HEAD's bytes, so
    // that question has the same answer as the diff just run, and converting
    // one result twice beats diffing the file twice.
    let index_changed: Vec<Range<u32>> = {
        let hunks = if base.index_hash == base.head_hash {
            changes_to_hunks(&result.changes, base_text, buffer_text)
        } else {
            let index_result = structural_diff::diff(index_text, buffer_text);
            changes_to_hunks(&index_result.changes, index_text, buffer_text)
        };
        hunks
            .into_iter()
            .map(|hunk| hunk.buffer_line_range)
            .collect()
    };

    let mut diff_map = DiffMap::from_structural_changes_staged(
        result,
        base.head.clone(),
        buffer_text,
        &index_changed,
    );
    if let Some(language) = language {
        diff_map.set_base_highlights(compute_base_highlights(
            base_text,
            language,
            syntax_styles,
            base_cache,
        ));
    }
    Some((diff_map, base.clone()))
}

/// Highlight `base_text` for the diff view's left column, per base line.
///
/// Memoized against `cache` twice over. An unchanged base text is parsed once,
/// and its spans are resolved and bucketed once per theme, so a keystroke burst
/// that leaves the base alone costs two hash lookups rather than a walk over
/// every span with a style clone per line it touches.
fn compute_base_highlights(
    base_text: &str,
    language: &Arc<Language>,
    syntax_styles: &SyntaxStyles,
    cache: &BaseHighlightCache,
) -> Arc<BaseHighlights> {
    let content: ContentHash = blake3::hash(base_text.as_bytes()).into();
    let name = language.name.to_string();
    let bucket_key = (content, name.clone(), syntax_styles.generation);

    let parse_key = (content, name);
    let hit = {
        let guard = cache.lock().expect("base highlight cache poisoned");
        if let Some(bucketed) = guard.buckets.get(&bucket_key) {
            return bucketed.clone();
        }
        guard.parses.get(&parse_key).cloned()
    };

    // Parsed outside the lock, which a miss holds only long enough to look up.
    // A changeset warms one diff job per changed file on the blocking pool, and
    // parsing under the lock queues every one of them behind whichever job got
    // there first.
    //
    // Two jobs missing at once both parse, which is the price. Same content and
    // same language means the same spans, so whichever lands first is kept and
    // the other is dropped.
    let spans = match hit {
        Some(spans) => spans,
        None => {
            let parsed = Arc::new(
                parse(language, base_text, None)
                    .map(|tree| extract_highlights(language, &tree, base_text))
                    .unwrap_or_default(),
            );
            cache
                .lock()
                .expect("base highlight cache poisoned")
                .parses
                .entry(parse_key)
                .or_insert(parsed)
                .clone()
        },
    };

    // Bucketed outside the lock, so one job's O(spans) resolve does not hold up
    // every other buffer's diff.
    let bucketed = Arc::new(bucket_base_highlights(&spans, base_text, syntax_styles));
    cache
        .lock()
        .expect("base highlight cache poisoned")
        .buckets
        .insert(bucket_key, bucketed.clone());
    bucketed
}

/// Resolve highlight spans to styles and bucket them per base line as line-local
/// byte ranges. A span crossing a newline is clipped to each line it touches.
fn bucket_base_highlights(
    spans: &[HighlightSpan],
    base_text: &str,
    syntax_styles: &SyntaxStyles,
) -> BaseHighlights {
    let starts = line_starts(base_text);
    let line_of = |byte: usize| starts.partition_point(|&s| s <= byte).saturating_sub(1);

    let mut per_line: BaseHighlights = vec![Vec::new(); starts.len()];
    for span in spans {
        let Some(style_id) = syntax_styles.id_for_highlight(span.id) else {
            continue;
        };
        let style = syntax_styles.interner[style_id].clone();

        let first = line_of(span.byte_range.start);
        let last = line_of(
            span.byte_range
                .end
                .saturating_sub(1)
                .max(span.byte_range.start),
        );
        for line in first..=last {
            let line_start = starts[line];
            let line_end = starts.get(line + 1).copied().unwrap_or(base_text.len());
            let s = span.byte_range.start.max(line_start) - line_start;
            let e = span.byte_range.end.min(line_end) - line_start;
            if s < e {
                per_line[line].push((s..e, style.clone()));
            }
        }
    }
    per_line
}

/// The working-tree byte ranges a file's hunks cover, diffing its HEAD text
/// against its working-tree text.
///
/// Hunk line ranges are converted to byte ranges in the working-tree text
/// so a symbol's byte def-range can be tested for overlap directly.
///
/// A deletion has no working-tree lines of its own, so it yields an empty range
/// at the seam it was removed from. That range is kept rather than dropped. The
/// overlap test the caller applies treats it as a point, which is how a
/// deletion marks the symbol it was cut out of.
///
/// Uses the line diff rather than the language-aware structural diff. The only
/// consumer tests whole-line overlap, and treating moved code as a delete plus
/// an add yields the same or a strictly larger changed set for that test, at a
/// fraction of the cost.
/// Diff every changed file against HEAD and collect the byte ranges its hunks
/// cover, reusing `memo` for a file whose base and buffer text both still hash
/// to what the ranges were measured from.
///
/// Free of the workspace so it runs on a blocking thread. The status walk, the
/// HEAD blobs, and a disk read per changed file are the whole cost of a
/// diff-filtered hop. [`Workspace::install_changed_ranges`] takes the result.
pub(crate) fn scan_changed_ranges(
    git: &dyn GitHost,
    fs: &dyn FsHost,
    langs: &LanguageRegistry,
    git_root: &Path,
    memo: &ChangedRangesMemo,
) -> ChangedRangesScan {
    let mut scan = ChangedRangesScan::default();
    let Some((_workdir, inputs)) = diff::scan_working_tree(git, fs, langs, git_root) else {
        return scan;
    };

    for input in &inputs {
        let fid = file_id(&input.rel_path);
        let base_hash = buffer_registry::fingerprint_bytes(input.base_text.as_str());
        let buffer_hash = buffer_registry::fingerprint_bytes(input.buffer_text.as_str());

        let ranges = match memo.get(&fid) {
            Some((cached_base, cached_buffer, cached))
                if *cached_base == base_hash && *cached_buffer == buffer_hash =>
            {
                cached.clone()
            },
            _ => {
                let computed = changed_byte_ranges(input);
                scan.computed
                    .push((fid, base_hash, buffer_hash, computed.clone()));
                computed
            },
        };

        if !ranges.is_empty() {
            scan.ranges.insert(fid, ranges);
        }
    }
    scan
}

fn changed_byte_ranges(input: &ReviewFileInput) -> Vec<Range<usize>> {
    let result = structural_diff::diff(&input.base_text, &input.buffer_text);
    let hunks = changes_to_hunks(&result.changes, &input.base_text, &input.buffer_text);
    let starts = line_starts(&input.buffer_text);

    let offset_of_row = |row: u32| {
        starts
            .get(row as usize)
            .copied()
            .unwrap_or(input.buffer_text.len())
    };

    hunks
        .iter()
        .map(|hunk| {
            offset_of_row(hunk.buffer_line_range.start)..offset_of_row(hunk.buffer_line_range.end)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        changed_byte_ranges, compute_base_highlights, scan_changed_ranges, BaseHighlightMemo,
    };
    use crate::{
        display_map::syntax_theme::SyntaxStyles, review::ReviewFileInput,
        test_harness::TestHarness, theme::Theme, workspace::Workspace,
    };
    use std::{
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };
    use stoat_language::LanguageRegistry;

    /// Bucketing the base spans is O(spans) with a style clone per line each
    /// span touches, and it ran per recompute over a base text and a style
    /// table that a keystroke burst leaves alone. Only a rebuilt style table
    /// can change the answer, so only that may cost the walk again.
    #[test]
    fn base_highlights_reuse_the_bucketed_map_until_the_styles_are_rebuilt() {
        let cache = Arc::new(Mutex::new(BaseHighlightMemo::default()));
        let language = LanguageRegistry::standard()
            .for_path(Path::new("a.rs"))
            .expect("rust language");
        let base = "fn main() {\n    let x = 1;\n}\n";
        let styles = SyntaxStyles::from_theme(&Theme::empty());

        let first = compute_base_highlights(base, &language, &styles, &cache);
        let second = compute_base_highlights(base, &language, &styles, &cache);
        assert!(
            Arc::ptr_eq(&first, &second),
            "the same style table serves the bucketed map it already built",
        );

        let rebuilt = SyntaxStyles::from_theme(&Theme::empty());
        let third = compute_base_highlights(base, &language, &rebuilt, &cache);
        assert!(
            !Arc::ptr_eq(&first, &third),
            "a rebuilt style table cannot reuse a resolve made against the old one",
        );
        assert_eq!(
            *first, *third,
            "and the miss is conservative: the same theme resolves the same way",
        );
    }

    fn input(base: &str, buffer: &str) -> ReviewFileInput {
        ReviewFileInput {
            path: PathBuf::from("/repo/a.rs"),
            rel_path: "a.rs".to_string(),
            language: None,
            base_text: Arc::new(base.to_string()),
            buffer_text: Arc::new(buffer.to_string()),
        }
    }

    #[test]
    fn changed_byte_ranges_covers_an_added_line() {
        let ranges = changed_byte_ranges(&input("fn foo() {}\n", "fn foo() {}\nfn bar() {}\n"));
        assert!(
            ranges.iter().any(|r| r.contains(&15)),
            "the added second line's bytes are reported changed, got {ranges:?}",
        );
    }

    #[test]
    fn changed_byte_ranges_empty_when_identical() {
        assert!(changed_byte_ranges(&input("fn foo() {}\n", "fn foo() {}\n")).is_empty());
    }

    /// Every hunk becomes one working-tree byte range, deletions included.
    ///
    /// A deletion has no working-tree lines of its own, so it lands as an empty
    /// range at the seam it was cut from, wherever that seam falls. The range
    /// is kept rather than dropped. The caller's overlap test reads an empty
    /// range as a point, so a symbol spanning the seam still reports changed,
    /// which is the whole signal a deletion has to give.
    ///
    /// The last case has no closing newline, so its range ends at the text's
    /// end rather than at a line start the table does not hold.
    ///
    /// Compared as pairs because an expected `[2..2]` reads to the compiler as
    /// a range that might have meant a repeat count.
    #[test]
    fn changed_byte_ranges_converts_every_hunk_including_deletion_seams() {
        let cases = [
            ("a\nb\nc\n", "a\nc\n", vec![(2, 2)]),
            ("a\nb\nc\n", "b\nc\n", vec![(0, 0)]),
            ("a\nb\nc\nd\n", "a\nc\nd\ne\n", vec![(2, 2), (6, 8)]),
            ("a\nb", "a\nc", vec![(2, 3)]),
        ];

        for (base, buffer, expected) in cases {
            let got: Vec<(usize, usize)> = changed_byte_ranges(&input(base, buffer))
                .into_iter()
                .map(|r| (r.start, r.end))
                .collect();
            assert_eq!(got, expected, "{base:?} -> {buffer:?}");
        }
    }

    #[test]
    fn changed_ranges_scan_memoizes_across_unchanged_scans() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario(
            "/repo",
            &[("a.rs", "fn foo() {}\n", "fn foo() {}\nfn bar() {}\n")],
        );

        let git = h.stoat.git_host.clone();
        let fs = h.stoat.fs_host.clone();
        let langs = h.stoat.language_registry.clone();
        let scan_and_install = |ws: &mut Workspace| {
            let scan = scan_changed_ranges(
                git.as_ref(),
                fs.as_ref(),
                &langs,
                &ws.git_root.clone(),
                &ws.changed_ranges_memo_snapshot(),
            );
            ws.install_changed_ranges(scan);
        };

        let ws = h.stoat.active_workspace_mut();

        scan_and_install(ws);
        assert_eq!(
            ws.changed_ranges_recomputes, 1,
            "the first scan diffs the changed file once"
        );
        assert!(
            !ws.changed_ranges.is_empty(),
            "the working-tree change is recorded"
        );

        scan_and_install(ws);
        assert_eq!(
            ws.changed_ranges_recomputes, 1,
            "a second scan over the unchanged tree reuses the memo, no re-diff"
        );
        assert!(
            !ws.changed_ranges.is_empty(),
            "the recorded change survives the memo hit"
        );
    }
}
