use super::{
    display_width, sampler, BlockPlacement, BlockProperties, BlockRowKind, BlockStyle, DisplayMap,
    DisplayPoint, DisplayRow, DisplaySnapshot, HighlightStyle, HighlightStyleInterner, InlayKind,
    InlayPoint, SemanticTokenHighlight, WrapSync,
};
use crate::{
    buffer::{BufferId, TextBuffer},
    diff_map::{DiffHunk, DiffHunkStatus, DiffMap},
    multi_buffer::MultiBuffer,
};
use std::{
    ops::Range,
    sync::{Arc, RwLock},
};
use stoat_scheduler::{Executor, TestScheduler};
use stoat_text::Point;

fn test_executor() -> Executor {
    Executor::new(Arc::new(TestScheduler::new()))
}

fn create_display_map(content: &str) -> DisplayMap {
    let buffer = TextBuffer::with_text(BufferId::new(0), content);
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared);
    DisplayMap::new(multi_buffer, test_executor(), crate::test_notify())
}

fn one_token(
    display_map: &DisplayMap,
) -> (Arc<[SemanticTokenHighlight]>, Arc<HighlightStyleInterner>) {
    let snap = display_map.multi_buffer.snapshot();
    let mut interner = HighlightStyleInterner::default();
    let style = interner.push(HighlightStyle::default());
    let tokens: Arc<[SemanticTokenHighlight]> = [SemanticTokenHighlight {
        range: snap.anchor_at(0, stoat_text::Bias::Right)
            ..snap.anchor_at(3, stoat_text::Bias::Left),
        style,
    }]
    .into_iter()
    .collect();
    (tokens, Arc::new(interner))
}

fn assert_moved_once(display_map: &DisplayMap, seen: &mut u64, what: &str) {
    assert_eq!(
        display_map.settings_generation,
        *seen + 1,
        "{what} moves the generation one step",
    );
    *seen = display_map.settings_generation;
}

/// Every setter that changes what a pane paints moves the generation, one
/// step per call.
///
/// No layer version sees any of these. A masked editor and an unmasked one
/// carry the same fold, inlay, wrap, and block versions, so a paint key
/// built from those alone would call them the same frame.
#[test]
fn every_setter_moves_the_settings_generation() {
    let mut dm = create_display_map("alpha\nbeta\ngamma\n");
    let (tokens, interner) = one_token(&dm);
    let buffer_id = BufferId::new(0);
    let mut seen = dm.settings_generation;

    dm.set_show_deleted_blocks(true);
    assert_moved_once(&dm, &mut seen, "set_show_deleted_blocks");

    dm.set_masked(true);
    assert_moved_once(&dm, &mut seen, "set_masked");

    dm.set_syntax_highlighting(false);
    assert_moved_once(&dm, &mut seen, "set_syntax_highlighting");

    dm.set_clip_at_line_ends(true);
    assert_moved_once(&dm, &mut seen, "set_clip_at_line_ends");

    dm.set_diagnostics_max_severity(Some(super::DiagnosticSeverity::Warning));
    assert_moved_once(&dm, &mut seen, "set_diagnostics_max_severity");

    dm.set_wrap_width(Some(20));
    assert_moved_once(&dm, &mut seen, "set_wrap_width");

    dm.set_conflict_padding_blocks(vec![BlockProperties::from_text(
        BlockPlacement::Below(0),
        vec!["pad".to_string()],
        BlockStyle::Fixed,
    )]);
    assert_moved_once(&dm, &mut seen, "set_conflict_padding_blocks");

    dm.set_semantic_token_highlights(buffer_id, tokens.clone(), interner.clone());
    assert_moved_once(&dm, &mut seen, "set_semantic_token_highlights");

    let channel =
        super::highlights::BufferSemanticTokens::new(tokens.clone(), interner.clone(), |_| 0);
    dm.set_semantic_token_channel(buffer_id, channel);
    assert_moved_once(&dm, &mut seen, "set_semantic_token_channel");

    dm.set_lsp_token_channel(buffer_id, dm.batched_token_channel(tokens, interner));
    assert_moved_once(&dm, &mut seen, "set_lsp_token_channel");

    dm.invalidate_semantic_highlights(buffer_id);
    assert_moved_once(&dm, &mut seen, "invalidate_semantic_highlights");

    let snap = dm.multi_buffer.snapshot();
    let range =
        snap.anchor_at(0, stoat_text::Bias::Right)..snap.anchor_at(5, stoat_text::Bias::Left);

    let key = super::highlights::HighlightKey::layer(
        super::highlights::HighlightLayer::DocumentHighlightRead,
    );
    dm.highlight_text(key, vec![range.clone()], HighlightStyle::default());
    assert_moved_once(&dm, &mut seen, "highlight_text");

    assert!(dm.clear_highlights(key), "the key was just installed");
    assert_moved_once(&dm, &mut seen, "clear_highlights");

    dm.set_lsp_folding_ranges(buffer_id, vec![(range, None)]);
    assert_moved_once(&dm, &mut seen, "set_lsp_folding_ranges");
}

/// A setter handed the value it already holds moves nothing.
///
/// Some of these are documented as safe to call every frame, so a
/// generation that moved on every call would defeat the cache it exists to
/// serve. Only the setters whose value compares cheaply can promise this.
#[test]
fn a_setter_handed_what_it_holds_moves_nothing() {
    let mut dm = create_display_map("alpha\nbeta\ngamma\n");
    dm.set_masked(true);
    dm.set_syntax_highlighting(false);
    dm.set_clip_at_line_ends(true);
    dm.set_diagnostics_max_severity(Some(super::DiagnosticSeverity::Warning));
    dm.set_wrap_width(Some(20));
    dm.set_show_deleted_blocks(true);
    let settled = dm.settings_generation;

    dm.set_masked(true);
    dm.set_syntax_highlighting(false);
    dm.set_clip_at_line_ends(true);
    dm.set_diagnostics_max_severity(Some(super::DiagnosticSeverity::Warning));
    dm.set_wrap_width(Some(20));
    dm.set_show_deleted_blocks(true);

    assert_eq!(
        dm.settings_generation, settled,
        "a second round of the same values changed nothing",
    );
}

/// Two snapshots with nothing between them share a paint version.
///
/// This is what a pane cache reads to skip a repaint, so it has to hold
/// across a frame where the editor did nothing to the display.
///
/// Turning syntax coloring off is the change that follows, because it
/// reaches the snapshot down the highlight-refresh path rather than through
/// a rebuild, which is the other place the generation is stamped.
#[test]
fn two_snapshots_over_an_unchanged_map_share_a_paint_version() {
    let mut dm = create_display_map("alpha\nbeta\ngamma\n");
    let first = dm.snapshot().paint_version();
    let second = dm.snapshot().paint_version();

    assert_eq!(second, first, "nothing changed between the two snapshots");

    dm.set_syntax_highlighting(false);
    let after = dm.snapshot().paint_version();
    assert_ne!(after, first, "and turning syntax coloring off changed it");
}

fn create_display_map_with_diff(content: &str, diff_map: DiffMap) -> DisplayMap {
    let mut buffer = TextBuffer::with_text(BufferId::new(0), content);
    buffer.diff_map = Some(diff_map);
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared);
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
    display_map.set_show_deleted_blocks(true);
    display_map
}

fn make_diff_with_deletion(
    after_line: u32,
    base_text: &str,
    byte_range: Range<usize>,
    _line_count: u32,
) -> DiffMap {
    let mut dm = DiffMap::default();
    dm.set_base_text(Arc::new(base_text.to_string()));
    dm.push_hunk(DiffHunk {
        status: DiffHunkStatus::Deleted,
        unstaged_lines: std::iter::once((after_line + 1)..(after_line + 1)).collect(),
        marked_rows: Vec::new(),
        buffer_start_line: after_line + 1,
        buffer_line_range: (after_line + 1)..(after_line + 1),
        base_byte_range: byte_range,
        anchor_range: None,
        token_detail: None,
    });
    dm
}

/// Recomputing a diff stamps a fresh version whether or not anything moved,
/// and every version bump used to drop and re-add every deleted-line block.
/// A keystroke that leaves the hunks alone should leave the blocks alone,
/// which the ids show directly since each insert mints a new one.
#[test]
fn a_diff_refresh_that_changes_no_hunk_keeps_the_same_blocks() {
    let base = "line1\ndeleted\nline2";
    let mut buffer = TextBuffer::with_text(BufferId::new(0), "line1\nline2");
    buffer.diff_map = Some(make_diff_with_deletion(0, base, 6..13, 1));
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared.clone());
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
    display_map.set_show_deleted_blocks(true);

    display_map.snapshot();
    let first = display_map.inserted_diff_block_ids.clone();
    assert_eq!(first.len(), 1, "the deleted hunk contributes one block");

    // A re-diff after an edit elsewhere finds the same hunks and puts them
    // in a map carrying a new version.
    shared.write().expect("poisoned").diff_map = Some(make_diff_with_deletion(0, base, 6..13, 1));
    display_map.snapshot();

    assert_eq!(
        display_map.inserted_diff_block_ids, first,
        "a refresh finding the same hunks re-splices nothing",
    );
}

/// A refresh that found one new hunk should cost one block, not a whole new
/// set. The ids show it: an untouched hunk keeps the block it already had.
#[test]
fn a_refresh_finding_a_new_hunk_keeps_the_other_blocks() {
    let base = "line1\ndeleted\nline2\ngone\nline3";
    let mut buffer = TextBuffer::with_text(BufferId::new(0), "line1\nline2\nline3");
    buffer.diff_map = Some(make_diff_with_deletion(0, base, 6..13, 1));
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared.clone());
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
    display_map.set_show_deleted_blocks(true);

    display_map.snapshot();
    let first = display_map.inserted_diff_block_ids.clone();
    assert_eq!(first.len(), 1);

    // The same hunk plus one further down the file.
    let mut grown = make_diff_with_deletion(0, base, 6..13, 1);
    grown.push_hunk(DiffHunk {
        status: DiffHunkStatus::Deleted,
        unstaged_lines: std::iter::once(2..2).collect(),
        marked_rows: Vec::new(),
        buffer_start_line: 2,
        buffer_line_range: 2..2,
        base_byte_range: 19..24,
        anchor_range: None,
        token_detail: None,
    });
    shared.write().expect("poisoned").diff_map = Some(grown);
    display_map.snapshot();

    let after = display_map.inserted_diff_block_ids.clone();
    assert_eq!(after.len(), 2, "the new hunk adds a block");
    assert_eq!(
        after[0], first[0],
        "the hunk that did not move keeps the block it had",
    );
}

#[test]
fn display_snapshot_version() {
    let mut dm = create_display_map("hello");
    let v1 = dm.snapshot().version();
    let v2 = dm.snapshot().version();
    assert_eq!(v1, v2);
}

/// A server re-answers the folding request on every settled edit, and for
/// a file whose structure nobody changed it answers the same ranges.
/// Installing that answer costs two crease-tree rebuilds and a
/// `settings_generation` bump, which invalidates every highlight endpoint
/// cache, so the answer has to be recognized before any of it runs.
#[test]
fn an_unchanged_folding_answer_installs_nothing() {
    let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nline1\nline2\n");
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared);
    let mut dm = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

    let ranges = {
        let snap = dm.multi_buffer.snapshot();
        vec![(
            snap.anchor_at(0, stoat_text::Bias::Right)..snap.anchor_at(5, stoat_text::Bias::Left),
            None,
        )]
    };
    dm.set_lsp_folding_ranges(BufferId::new(0), ranges.clone());
    dm.snapshot();

    let installed = (dm.crease_map.tree_rebuilds(), dm.settings_generation);
    dm.set_lsp_folding_ranges(BufferId::new(0), ranges);

    assert_eq!(
        (dm.crease_map.tree_rebuilds(), dm.settings_generation),
        installed,
        "the same ranges are already installed",
    );

    // A different answer still installs, which is what says the compare
    // measures something.
    let moved = {
        let snap = dm.multi_buffer.snapshot();
        vec![(
            snap.anchor_at(6, stoat_text::Bias::Right)..snap.anchor_at(11, stoat_text::Bias::Left),
            None,
        )]
    };
    dm.set_lsp_folding_ranges(BufferId::new(0), moved);

    assert_ne!(
        (dm.crease_map.tree_rebuilds(), dm.settings_generation),
        installed,
        "and a real change reaches the tree",
    );
}

#[test]
fn crease_sync_tracks_only_buffer_edits() {
    let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nline1\nline2\n");
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared.clone());
    let mut dm = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

    let range = {
        let snap = dm.multi_buffer.snapshot();
        snap.anchor_at(0, stoat_text::Bias::Right)..snap.anchor_at(5, stoat_text::Bias::Left)
    };
    dm.set_lsp_folding_ranges(BufferId::new(0), vec![(range, None)]);

    dm.snapshot();
    let synced_at = dm.last_crease_sync_version;

    dm.insert_blocks(Vec::new());
    dm.snapshot();
    assert_eq!(
        dm.last_crease_sync_version, synced_at,
        "a rebuild with an unchanged buffer version does not re-sync creases",
    );

    {
        let mut buf = shared.write().unwrap();
        buf.edit(0..0, "x");
    }
    dm.snapshot();
    assert!(
        dm.last_crease_sync_version > synced_at,
        "a buffer edit re-syncs the crease map",
    );
}

#[test]
fn passthrough_coordinates() {
    let mut display_map = create_display_map("hello\nworld\n");
    let snapshot = display_map.snapshot();

    let buffer_point = Point::new(1, 3);
    let display_point = snapshot.buffer_to_display(buffer_point);
    assert_eq!(display_point, DisplayPoint::new(1, 3));

    let back = snapshot.display_to_buffer(display_point);
    assert_eq!(back, Some(buffer_point));
}

#[test]
fn line_count() {
    let mut display_map = create_display_map("line1\nline2\nline3");
    let snapshot = display_map.snapshot();
    assert_eq!(snapshot.line_count(), 3);
}

#[test]
fn line_count_survives_successive_mid_buffer_inserts() {
    let buffer = TextBuffer::with_text(BufferId::new(0), "aaaa\nbbbb\ncccc\ndddd\n");
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared.clone());
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

    let before = display_map.snapshot().line_count();
    assert_eq!(
        before, 5,
        "five display rows including the trailing phantom row"
    );

    for ch in ["x", "y"] {
        {
            let mut buf = shared.write().unwrap();
            buf.edit(6..6, ch);
        }
        assert_eq!(
            display_map.snapshot().line_count(),
            before,
            "a mid-buffer insert must not drop a display row",
        );
    }
}

#[test]
fn typing_mid_buffer_keeps_the_last_line_rendered() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 10);
    let path = h.write_file("edit.txt", "alpha\nbravo\ncharlie\ndelta\n");
    h.open_file(&path);

    h.type_keys("j j i");
    h.type_text("XY");

    let frame = h.snapshot();
    assert!(
        frame.content.contains("delta"),
        "the last line stays rendered while typing mid-buffer:\n{}",
        frame.content,
    );
}

/// The review and conflict paints resolve endpoints once per frame and
/// chunk each row against them, so what a repaint costs turns on whether
/// that resolve is reused. A hit hands back the same allocation, which is
/// the only way to see the difference from outside.
#[test]
fn a_repaint_that_changed_nothing_reuses_its_endpoints() {
    let buffer = TextBuffer::with_text(BufferId::new(0), "let x = 1\nlet y = 2\n");
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared.clone());
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

    let mut cache = None;
    let first = display_map
        .snapshot()
        .highlighted_endpoints_cached(0..2, &mut cache);
    let second = display_map
        .snapshot()
        .highlighted_endpoints_cached(0..2, &mut cache);
    assert!(
        Arc::ptr_eq(&first, &second),
        "an unchanged frame reuses the resolve",
    );

    // A different range describes different bytes, so it cannot answer from
    // the same set.
    let narrower = display_map
        .snapshot()
        .highlighted_endpoints_cached(0..1, &mut cache);
    assert!(!Arc::ptr_eq(&first, &narrower), "a new range rebuilds");

    shared.write().expect("poisoned").edit(0..0, "// edit\n");
    let after_edit = display_map
        .snapshot()
        .highlighted_endpoints_cached(0..1, &mut cache);
    assert!(
        !Arc::ptr_eq(&narrower, &after_edit),
        "a buffer edit rebuilds",
    );
}

/// Blocks at rows `5..5 + count`, replacing whatever the last call left.
fn padding_blocks(map: &mut DisplayMap, count: u32) {
    map.set_conflict_padding_blocks(
        (0..count)
            .map(|i| {
                BlockProperties::from_text(
                    BlockPlacement::Below(5 + i),
                    vec![format!("pad{i}")],
                    BlockStyle::Fixed,
                )
            })
            .collect(),
    );
}

/// The carry and the block list run once per sync over a set that does not
/// change size from one keystroke to the next, so they hold their working
/// room rather than asking for it again each time.
///
/// Shrinking the block set is what makes the reuse visible. Room sized for
/// the wide set outliving the narrow one can only have come from the wide
/// call, since a buffer built per call is sized to its own call.
#[test]
fn a_sync_reuses_the_room_the_previous_sync_asked_for() {
    let text: String = (0..60).map(|i| format!("line{i}\n")).collect();
    let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
    let mut display_map = DisplayMap::new(
        MultiBuffer::singleton(shared.clone()),
        test_executor(),
        crate::test_notify(),
    );

    padding_blocks(&mut display_map, 40);
    display_map.snapshot();

    shared.write().expect("poisoned").edit(0..0, "a\n");
    display_map.snapshot();

    let carry_room = display_map.block_map.carry_row_capacity();
    let block_room = display_map.block_map.block_list_capacity();
    assert!(
        carry_room >= 80,
        "40 blocks carry a start and an end each, so the carry read 80 rows, not {carry_room}",
    );
    assert!(
        block_room >= 40,
        "the sync listed 40 blocks, not {block_room}",
    );

    padding_blocks(&mut display_map, 2);
    shared.write().expect("poisoned").edit(0..0, "b\n");
    display_map.snapshot();

    assert_eq!(
        display_map.block_map.carry_row_capacity(),
        carry_room,
        "a carry over 2 blocks keeps the room the 40-block carry took",
    );
    assert_eq!(
        display_map.block_map.block_list_capacity(),
        block_room,
        "a sync listing 2 blocks keeps the room the 40-block sync took",
    );
}

#[test]
fn insert_blocks_after_snapshot_grows_line_count() {
    let mut display_map = create_display_map("line1\nline2\nline3");
    // Prime the snapshot cache, then insert a one-row block. The next
    // snapshot must rebuild and reflect the added row rather than return
    // the cached snapshot taken before the insert.
    assert_eq!(display_map.snapshot().line_count(), 3);

    display_map.insert_blocks(vec![BlockProperties::from_text(
        BlockPlacement::Below(0),
        vec!["extra".to_string()],
        BlockStyle::Fixed,
    )]);

    assert_eq!(display_map.snapshot().line_count(), 4);
}

/// Many blocks shifted by one edit land where a from-scratch build puts them.
///
/// Where a single block ends up is a weak check. A sync that leaves the
/// transform tree stale can still get one block right. Comparing every row of
/// an incrementally-synced map against a map built fresh over the same final
/// text is what pins the tree itself, and it is the shape that catches a
/// block silently dropped at a rebuild region's boundary.
#[test]
fn many_blocks_shifted_by_an_edit_match_a_fresh_build() {
    let text: String = (0..60).map(|i| format!("line{i}\n")).collect();

    // Anchored below row 5 and up, so the edit at the top slides every one
    // of them and collapses none. A block inside the edited range has its
    // own behaviour, covered by the sibling tests.
    let blocks_at = |shift: u32| -> Vec<BlockProperties> {
        (5..55)
            .map(|i| {
                BlockProperties::from_text(
                    BlockPlacement::Below(i + shift),
                    vec![format!("marker{i}")],
                    BlockStyle::Fixed,
                )
            })
            .collect()
    };

    let rows = |map: &mut DisplayMap| {
        let snapshot = map.snapshot();
        (0..snapshot.line_count())
            .map(|row| match snapshot.classify_row(row) {
                BlockRowKind::BufferRow { buffer_row } => format!("buf{buffer_row}"),
                BlockRowKind::Block { block, line_index } => block.get_line(line_index).to_string(),
            })
            .collect::<Vec<_>>()
    };

    let fresh = |content: &str, shift: u32| {
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            content,
        )));
        let multi = MultiBuffer::singleton(shared);
        let mut map = DisplayMap::new(multi, test_executor(), crate::test_notify());
        map.insert_blocks(blocks_at(shift));
        rows(&mut map)
    };

    let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
    let multi_buffer = MultiBuffer::singleton(shared.clone());
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
    display_map.insert_blocks(blocks_at(0));
    assert_eq!(rows(&mut display_map), fresh(&text, 0), "before any edit");

    shared.write().expect("poisoned").edit(0..0, "a\nb\nc\n");
    let inserted = format!("a\nb\nc\n{text}");
    assert_eq!(
        rows(&mut display_map),
        fresh(&inserted, 3),
        "three rows inserted above all 50 blocks, which carry down three rows",
    );

    shared.write().expect("poisoned").edit(0..6, "");
    assert_eq!(
        rows(&mut display_map),
        fresh(&text, 0),
        "and removed again, back to the original",
    );
}

/// An edit appending a row at the very end leaves the map the same length
/// as a fresh build.
///
/// The row patch each layer hands down says which rows changed and how many
/// replaced them. A region running to the end of the buffer has to count
/// the empty row after the final newline, which the accumulated position
/// stops short of, so a patch built from that position alone reports the
/// appended row as a same-size change and the layers above build one row
/// short.
#[test]
fn an_insert_at_the_buffer_end_adds_a_row() {
    let text: String = (0..5).map(|i| format!("line{i}\n")).collect();
    let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
    let multi = MultiBuffer::singleton(shared.clone());
    let mut map = DisplayMap::new(multi, test_executor(), crate::test_notify());
    assert_eq!(
        map.snapshot().line_count(),
        6,
        "five lines and the empty one"
    );

    let len = shared.read().expect("poisoned").rope().len();
    shared.write().expect("poisoned").edit(len..len, "zz\n");

    let fresh_multi = MultiBuffer::singleton(shared.clone());
    let mut fresh = DisplayMap::new(fresh_multi, test_executor(), crate::test_notify());
    assert_eq!(
        map.snapshot().line_count(),
        fresh.snapshot().line_count(),
        "the appended row reaches the incremental map too",
    );
}

/// Splicing an inlay onto a row above a fold leaves the fold collapsed.
///
/// A splice reports the row it lands on as changed, and the region rebuilt
/// for it ends where the next row begins. A fold starting on exactly that
/// row sits outside the region, so the rebuild does not re-emit it and the
/// old transform tree is expected to carry it. The transform the cursor
/// rests on at that point is the fold's own placeholder, and re-emitting
/// what follows the region as ordinary text unfolds it.
#[test]
fn an_inlay_spliced_above_a_fold_leaves_it_folded() {
    let text: String = (0..30).map(|i| format!("line{i}\n")).collect();
    let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
    let multi = MultiBuffer::singleton(shared.clone());
    let mut map = DisplayMap::new(multi, test_executor(), crate::test_notify());

    map.fold(vec![Point::new(5, 0)..Point::new(8, 0)]);
    let folded = map.snapshot().fold_snapshot().line_count();
    assert_eq!(folded, 28, "three rows collapsed out of thirty-one");

    let anchor = {
        let snap = map.multi_buffer.snapshot();
        let offset = snap.rope().point_to_offset(Point::new(4, 0));
        snap.anchor_at(offset, stoat_text::Bias::Left)
    };
    map.splice_inlays(
        Vec::new(),
        vec![(anchor, ": u32".to_string(), InlayKind::Hint)],
    );

    assert_eq!(
        map.snapshot().fold_snapshot().line_count(),
        folded,
        "the inlay carries no newline, so no row is added or removed",
    );

    // The line count alone would pass a tree that lost the fold and gained
    // it back elsewhere, and it is the rows that reach the reader.
    let rows = |map: &mut DisplayMap| {
        let snapshot = map.snapshot();
        (0..snapshot.line_count())
            .map(|row| snapshot.display_line(row))
            .collect::<Vec<_>>()
    };

    let fresh = |shared: &Arc<RwLock<TextBuffer>>| {
        let multi = MultiBuffer::singleton(shared.clone());
        let mut fresh = DisplayMap::new(multi, test_executor(), crate::test_notify());
        fresh.fold(vec![Point::new(5, 0)..Point::new(8, 0)]);
        fresh.splice_inlays(
            Vec::new(),
            vec![(anchor, ": u32".to_string(), InlayKind::Hint)],
        );
        rows(&mut fresh)
    };
    assert_eq!(rows(&mut map), fresh(&shared), "against a fresh build");

    // An unfolded tree survives its own sync, so the divergence only
    // reaches the reader on the next one.
    let len = shared.read().expect("poisoned").rope().len();
    shared.write().expect("poisoned").edit(len..len, "tail\n");
    assert_eq!(rows(&mut map), fresh(&shared), "and after a later edit");
}

/// A fold follows its text across an edit that removes a row above it
/// without changing the buffer's length.
///
/// Replacing a newline with an ordinary character costs the same bytes it
/// frees, so every offset after it stays exactly where it was while the row
/// it ended disappears and each later row shifts up by one. A fold is stored
/// by offset and resolved into rows, so this is the one edit shape that
/// leaves the stored form untouched and the resolved form moved.
#[test]
fn a_fold_below_a_byte_neutral_row_merge_moves_up_a_row() {
    let text: String = (0..30).map(|i| format!("line{i} words\n")).collect();
    let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
    let multi = MultiBuffer::singleton(shared.clone());
    let mut map = DisplayMap::new(multi, test_executor(), crate::test_notify());

    map.fold(vec![Point::new(15, 0)..Point::new(16, 0)]);
    let before = map.snapshot();
    assert_eq!(
        before.line_count(),
        30,
        "one of thirty-one rows folded away"
    );

    // The newline ending row 2, overwritten in place.
    let at = {
        let snap = shared.read().expect("poisoned");
        snap.rope().point_to_offset(Point::new(3, 0)) - 1
    };
    let len_before = shared.read().expect("poisoned").rope().len();
    shared.write().expect("poisoned").edit(at..at + 1, "X");
    assert_eq!(
        shared.read().expect("poisoned").rope().len(),
        len_before,
        "the edit has to be byte-neutral or it proves nothing",
    );

    let rows = |map: &mut DisplayMap| {
        let snapshot = map.snapshot();
        (0..snapshot.line_count())
            .map(|row| snapshot.display_line(row))
            .collect::<Vec<_>>()
    };

    let fresh_rows = {
        let multi = MultiBuffer::singleton(shared.clone());
        let mut fresh = DisplayMap::new(multi, test_executor(), crate::test_notify());
        fresh.fold(vec![Point::new(14, 0)..Point::new(15, 0)]);
        rows(&mut fresh)
    };
    assert_eq!(rows(&mut map), fresh_rows, "against a fresh build");
}

/// Two edits in one patch, the first ending where a replacement begins and
/// the second landing inside it, leave the block standing.
///
/// A region rebuild scans the blocks its rows cover and remembers where the
/// scan reached, so the next region in the same patch starts from there
/// rather than from the beginning. A block starting exactly at a region's
/// end is outside it, since the test is strict, but it is the next region
/// that owns it. Counting it as scanned leaves it owned by no region at
/// all, and a replacement is not carried over from the old tree either,
/// because the edit inside it consumed its transform.
#[test]
fn a_replacement_survives_an_edit_after_one_ending_at_its_start() {
    let text: String = (0..32).map(|i| format!("line{i} words\n")).collect();

    let block = || {
        vec![BlockProperties::from_text(
            BlockPlacement::Replace { start: 26, end: 28 },
            vec!["replacement".to_string()],
            BlockStyle::Fixed,
        )]
    };

    let rows = |map: &mut DisplayMap| {
        let snapshot = map.snapshot();
        (0..snapshot.line_count())
            .map(|row| match snapshot.classify_row(row) {
                BlockRowKind::BufferRow { buffer_row } => format!("buf{buffer_row}"),
                BlockRowKind::Block { block, line_index } => block.get_line(line_index),
            })
            .collect::<Vec<_>>()
    };

    let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
    let multi = MultiBuffer::singleton(shared.clone());
    let mut map = DisplayMap::new(multi, test_executor(), crate::test_notify());
    map.insert_blocks(block());
    map.snapshot();

    // Both land in one patch, so the second region's block scan starts
    // where the first region's left off. Same-size so no row moves.
    for row in [14u32, 27] {
        let at = {
            let snap = shared.read().expect("poisoned");
            snap.rope().point_to_offset(Point::new(row, 2))
        };
        shared.write().expect("poisoned").edit(at..at + 2, "QQ");
    }

    let fresh_rows = {
        let multi = MultiBuffer::singleton(shared.clone());
        let mut fresh = DisplayMap::new(multi, test_executor(), crate::test_notify());
        fresh.insert_blocks(block());
        rows(&mut fresh)
    };
    assert!(
        fresh_rows.contains(&"replacement".to_string()),
        "the fresh build has to show the block, or this proves nothing",
    );
    assert_eq!(rows(&mut map), fresh_rows, "against a fresh build");
}

/// An edit strictly inside a replacement's hidden rows leaves the block
/// standing.
///
/// Such an edit moves neither endpoint, so nothing carries the block over
/// and the region has to be rebuilt. The rebuild begins wherever the edit
/// does, which is inside the block, so the rows it hides get re-emitted as
/// ordinary text and the block itself is never put back.
#[test]
fn an_edit_inside_a_replacement_keeps_the_block() {
    let text: String = (0..10).map(|i| format!("line{i}\n")).collect();

    let block = || {
        vec![BlockProperties::from_text(
            BlockPlacement::Replace { start: 2, end: 6 },
            vec!["replacement".to_string()],
            BlockStyle::Fixed,
        )]
    };

    let rows = |map: &mut DisplayMap| {
        let snapshot = map.snapshot();
        (0..snapshot.line_count())
            .map(|row| match snapshot.classify_row(row) {
                BlockRowKind::BufferRow { buffer_row } => format!("buf{buffer_row}"),
                BlockRowKind::Block { block, line_index } => block.get_line(line_index).to_string(),
            })
            .collect::<Vec<_>>()
    };

    let fresh = |content: &str| {
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            content,
        )));
        let multi = MultiBuffer::singleton(shared);
        let mut map = DisplayMap::new(multi, test_executor(), crate::test_notify());
        map.insert_blocks(block());
        rows(&mut map)
    };

    let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
    let multi_buffer = MultiBuffer::singleton(shared.clone());
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
    display_map.insert_blocks(block());
    assert_eq!(rows(&mut display_map), fresh(&text), "before any edit");

    // Row 4 is the third of the five hidden rows, so this touches neither
    // endpoint of the replacement. Same length, so no row count changes.
    let edited = text.replace("line4", "LINE4");
    shared.write().expect("poisoned").edit(24..29, "LINE4");
    assert_eq!(
        rows(&mut display_map),
        fresh(&edited),
        "an edit between the replacement's endpoints leaves it standing",
    );
}

/// A block marks a row, not a row number, so an edit that moves the text
/// under it has to move the block with it. Otherwise it stays where the
/// row used to be and marks whatever slid into its place, and only the
/// block's owner re-inserting it can put it back.
#[test]
fn blocks_follow_the_row_they_mark_across_an_edit() {
    let text: String = (0..20).map(|i| format!("line{i}\n")).collect();
    let buffer = TextBuffer::with_text(BufferId::new(0), &text);
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared.clone());
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

    display_map.insert_blocks(vec![BlockProperties::from_text(
        BlockPlacement::Below(10),
        vec!["marker".to_string()],
        BlockStyle::Fixed,
    )]);

    let block_row = |map: &mut DisplayMap| {
        let snapshot = map.snapshot();
        (0..snapshot.line_count())
            .find(|row| {
                matches!(
                    snapshot.classify_row(*row),
                    BlockRowKind::Block { block, line_index }
                        if block.get_line(line_index) == "marker"
                )
            })
            .expect("the marker block is rendered")
    };

    assert_eq!(
        block_row(&mut display_map),
        11,
        "below line10 to begin with"
    );

    shared.write().expect("poisoned").edit(0..0, "a\nb\nc\n");

    assert_eq!(
        block_row(&mut display_map),
        14,
        "three rows inserted above it push the block down with line10",
    );

    // "a\nb\nc\n" back off the front again.
    shared.write().expect("poisoned").edit(0..6, "");

    assert_eq!(
        block_row(&mut display_map),
        11,
        "deleting those rows pulls it back up",
    );
}

/// The rows a block was attached to can be replaced wholesale, leaving it
/// nothing to point at. It collapses to the start of the replacement rather
/// than keeping a row number that now belongs to unrelated text.
#[test]
fn a_block_inside_a_replaced_range_lands_at_the_replacement() {
    let text: String = (0..20).map(|i| format!("line{i}\n")).collect();
    let buffer = TextBuffer::with_text(BufferId::new(0), &text);
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared.clone());
    let rows = MultiBuffer::singleton(shared.clone());
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

    display_map.insert_blocks(vec![BlockProperties::from_text(
        BlockPlacement::Below(10),
        vec!["marker".to_string()],
        BlockStyle::Fixed,
    )]);
    assert_eq!(display_map.snapshot().line_count(), 22);

    // Rows 8 through 12 replaced by a single line.
    let (start, end) = {
        let snapshot = rows.snapshot();
        let rope = snapshot.rope();
        (
            rope.point_to_offset(Point::new(8, 0)),
            rope.point_to_offset(Point::new(13, 0)),
        )
    };
    shared.write().expect("poisoned").edit(start..end, "only\n");

    let snapshot = display_map.snapshot();
    let block_row = (0..snapshot.line_count())
        .find(|row| {
            matches!(
                snapshot.classify_row(*row),
                BlockRowKind::Block { block, line_index }
                    if block.get_line(line_index) == "marker"
            )
        })
        .expect("the marker block is still rendered");

    assert_eq!(
        block_row, 9,
        "the block sits just below row 8, where its rows were replaced",
    );
}

#[test]
fn max_point() {
    let mut display_map = create_display_map("short\nlonger line\nx");
    let snapshot = display_map.snapshot();

    let max = snapshot.max_point();
    assert_eq!(max.row, 2);
    assert_eq!(max.column, 1);
}

#[test]
fn display_row_default() {
    let row = DisplayRow::default();
    assert_eq!(row.0, 0);
}

#[test]
fn line_count_includes_deleted() {
    let base = "line1\ndeleted\nline2";
    let diff = make_diff_with_deletion(0, base, 6..13, 1);
    let mut display_map = create_display_map_with_diff("line1\nline2", diff);
    let snapshot = display_map.snapshot();

    assert_eq!(snapshot.line_count(), 3);
    assert_eq!(snapshot.buffer_line_count(), 2);
}

#[test]
fn deleted_blocks_hidden_when_flag_off() {
    let base = "line1\ndeleted\nline2";
    let diff = make_diff_with_deletion(0, base, 6..13, 1);
    let mut buffer = TextBuffer::with_text(BufferId::new(0), "line1\nline2");
    buffer.diff_map = Some(diff);
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared);
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
    let snapshot = display_map.snapshot();

    assert_eq!(
        snapshot.line_count(),
        snapshot.buffer_line_count(),
        "with show_deleted_blocks off, a deletion diff splices no block rows",
    );
}

#[test]
fn classify_deleted_row() {
    let base = "line1\ndeleted\nline2";
    let diff = make_diff_with_deletion(0, base, 6..13, 1);
    let mut display_map = create_display_map_with_diff("line1\nline2", diff);
    let snapshot = display_map.snapshot();

    match snapshot.classify_row(1) {
        BlockRowKind::Block { block, line_index } => {
            assert_eq!(block.get_line(line_index), "deleted");
        },
        _ => panic!("expected block"),
    }
}

#[test]
fn roundtrip_with_tabs() {
    let mut display_map = create_display_map("\thello");
    let snapshot = display_map.snapshot();

    let display = snapshot.buffer_to_display(Point::new(0, 1));
    assert_eq!(display, DisplayPoint::new(0, 4));

    let back = snapshot.display_to_buffer(display).unwrap();
    assert_eq!(back, Point::new(0, 1));

    let display5 = DisplayPoint::new(0, 5);
    let back5 = snapshot.display_to_buffer(display5).unwrap();
    assert_eq!(back5, Point::new(0, 2));
}

#[test]
fn roundtrip_with_folds() {
    let mut display_map = create_display_map("fn main() {\n    body;\n}");
    display_map.fold(vec![Point::new(0, 11)..Point::new(2, 0)]);
    let snapshot = display_map.snapshot();

    let display = snapshot.buffer_to_display(Point::new(2, 1));
    let back = snapshot.display_to_buffer(display).unwrap();
    assert_eq!(back, Point::new(2, 1));
}

#[test]
fn line_len_display() {
    let mut display_map = create_display_map("\thello\nworld");
    let snapshot = display_map.snapshot();

    assert_eq!(snapshot.line_len(0), 9);
    assert_eq!(snapshot.line_len(1), 5);
}

#[test]
fn clip_point_clamps() {
    use stoat_text::Bias;
    let mut display_map = create_display_map("hello\nhi");
    let snapshot = display_map.snapshot();

    assert_eq!(
        snapshot.clip_point(DisplayPoint::new(0, 100), Bias::Left),
        DisplayPoint::new(0, 5)
    );
    assert_eq!(
        snapshot.clip_point(DisplayPoint::new(10, 0), Bias::Left),
        DisplayPoint::new(1, 0)
    );
}

#[test]
fn clip_point_moves_off_a_fold_placeholder() {
    use stoat_text::Bias;
    let mut display_map = create_display_map("fn main() {\n    body;\n}");
    display_map.toggle_fold(vec![Point::new(0, 11)..Point::new(2, 0)]);
    let snapshot = display_map.snapshot();

    assert_eq!(
        snapshot.clip_point(DisplayPoint::new(0, 12), Bias::Left),
        DisplayPoint::new(0, 11),
        "a column inside the placeholder falls back to where the fold opens",
    );
    assert_eq!(
        snapshot.clip_point(DisplayPoint::new(0, 12), Bias::Right),
        DisplayPoint::new(0, 14),
        "and forward to the character the fold closes before",
    );
}

#[test]
fn clip_point_moves_off_a_tab_expansion() {
    use stoat_text::Bias;
    let mut display_map = create_display_map("\thello");
    let snapshot = display_map.snapshot();

    assert_eq!(
        snapshot.clip_point(DisplayPoint::new(0, 2), Bias::Left),
        DisplayPoint::new(0, 0),
        "a column among the tab's cells falls back to the tab",
    );
    assert_eq!(
        snapshot.clip_point(DisplayPoint::new(0, 2), Bias::Right),
        DisplayPoint::new(0, 4),
        "and forward to the stop it runs to",
    );
}

#[test]
fn clip_point_moves_out_of_the_soft_wrap_indent() {
    use stoat_text::Bias;
    let mut display_map = create_display_map("    hello world example text");
    display_map.set_wrap_width(Some(12));
    let snapshot = display_map.snapshot();

    let indent = snapshot.soft_wrap_indent(1);
    assert!(indent > 0, "the continuation row carries an indent");
    assert_eq!(
        snapshot.clip_point(DisplayPoint::new(1, 0), Bias::Left),
        DisplayPoint::new(1, indent),
        "the margin holds no text, so a column in it clamps to the first cell",
    );
}

/// Clipping names a position the rest of the display map agrees exists.
/// A clipped point converts to a buffer point and back to itself, and
/// clipping it again leaves it alone.
#[test]
fn clip_point_settles_on_an_addressable_position() {
    use stoat_text::Bias;
    let mut display_map = create_display_map("fn main() {\n\tbody;\n}");
    display_map.toggle_fold(vec![Point::new(1, 1)..Point::new(1, 5)]);
    let snapshot = display_map.snapshot();

    for row in 0..snapshot.line_count() {
        for column in 0..=snapshot.line_len(row) + 1 {
            for bias in [Bias::Left, Bias::Right] {
                let clipped = snapshot.clip_point(DisplayPoint::new(row, column), bias);
                assert_eq!(
                    snapshot.clip_point(clipped, bias),
                    clipped,
                    "clipping {row}:{column} again moves it",
                );

                let buffer_point = snapshot
                    .display_to_buffer(clipped)
                    .unwrap_or_else(|| panic!("{row}:{column} clips off the buffer"));
                assert_eq!(
                    snapshot.buffer_to_display(buffer_point),
                    clipped,
                    "{row}:{column} does not survive the buffer round trip",
                );
            }
        }
    }
}

#[test]
fn toggle_fold_folds_then_unfolds() {
    let mut display_map = create_display_map("fn main() {\n    body;\n}");
    let range = vec![Point::new(0, 11)..Point::new(2, 0)];

    display_map.toggle_fold(range.clone());
    let snapshot = display_map.snapshot();
    assert_eq!(snapshot.line_count(), 1);

    display_map.toggle_fold(range);
    let snapshot = display_map.snapshot();
    assert_eq!(snapshot.line_count(), 3);
}

/// The chunk stream is what every paint reads, so any change to how it is
/// walked has to leave it identical. Each layer contributes its own way for
/// a restart to differ from a continuation. A wrapped row splits mid-chunk,
/// a fold and a hint move the offsets the layers below are addressed by, and
/// a block interrupts the run entirely.
#[test]
fn the_chunk_stream_over_wraps_folds_inlays_and_blocks() {
    // Row 1 is indented, so its continuation rows carry an indent. Row 3 is
    // long enough to wrap after the fold collapses part of it.
    let text = "fn alpha() { let x = 1; }\n\
                    \x20   indented line that wraps a few times\n\
                    short\n\
                    fn gamma() { let z = 3; } and more text\n";
    let buffer = TextBuffer::with_text(BufferId::new(0), text);
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared.clone());
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

    let hint_at = {
        let snap = display_map.multi_buffer.snapshot();
        snap.anchor_at(
            snap.rope().point_to_offset(Point::new(0, 12)),
            stoat_text::Bias::Right,
        )
    };
    display_map.splice_inlays(
        Vec::new(),
        vec![(hint_at, ": u32".to_string(), InlayKind::Hint)],
    );
    display_map.fold(vec![Point::new(3, 12)..Point::new(3, 24)]);
    display_map.insert_blocks(vec![BlockProperties::from_text(
        BlockPlacement::Below(1),
        vec!["a block row".to_string(), "and another".to_string()],
        BlockStyle::Fixed,
    )]);
    display_map.set_wrap_width(Some(14));

    let snapshot = display_map.snapshot();
    let stream: Vec<String> = snapshot
        .highlighted_chunks(0..snapshot.line_count())
        .map(|chunk| {
            format!(
                "{:?}{}{}",
                chunk.text,
                if chunk.is_inlay { " inlay" } else { "" },
                if chunk.is_tab { " tab" } else { "" },
            )
        })
        .collect();

    assert_eq!(
        stream.join("\n"),
        [
            // Row 0 wraps mid-hint, so the hint arrives as two chunks
            // either side of the break. The hint's cells count toward the
            // width, filling the first sub-row exactly and pushing the rest
            // of the line onto a third.
            r#""fn alpha() {""#,
            r#"": " inlay"#,
            r#""\n""#,
            r#""u32" inlay"#,
            r#"" let x = ""#,
            r#""\n""#,
            r#""1; }""#,
            r#""\n""#,
            // Row 1's first sub-row, then the block splitting it.
            r#""    indented ""#,
            r#""\n""#,
            r#""a block row""#,
            r#""\n""#,
            r#""and another""#,
            r#""\n""#,
            // Its remaining sub-rows, each opening with the carried indent.
            r#""    ""#,
            r#""line that ""#,
            r#""\n""#,
            r#""    ""#,
            r#""wraps a ""#,
            r#""\n""#,
            r#""    ""#,
            r#""few times""#,
            r#""\n""#,
            r#""short""#,
            r#""\n""#,
            // Row 3, whose fold placeholder is a chunk of its own.
            r#""fn gamma() ""#,
            r#""\n""#,
            r#""{""#,
            r#""...""#,
            r#""} and ""#,
            r#""\n""#,
            r#""more text""#,
            r#""\n""#,
        ]
        .join("\n"),
    );
}

/// A row's measured width is the width of the row that gets painted.
///
/// Every caret position, wrap break and horizontal scroll bound is derived
/// from the measurement, while what the user sees comes from the chunks. If
/// the two disagree the caret sits some number of cells away from its own
/// character, and the disagreement is invisible until something is wide:
/// each layer between them can widen a row, so the fixture carries one of
/// each.
#[test]
fn every_row_measures_the_width_it_paints() {
    // A tab, a hint, a fold, glyphs two cells wide, and a line long enough
    // to wrap. Row 2 puts the hint and the fold on one row so their offsets
    // compound.
    let text = "\tfn alpha() {}\n\
                    \u{4e00}\u{4e01} wide glyphs here\n\
                    fn beta() { let y = 2; } trailing words to force a wrap\n";
    let buffer = TextBuffer::with_text(BufferId::new(0), text);
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared);
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

    let hint_at = {
        let snap = display_map.multi_buffer.snapshot();
        snap.anchor_at(
            snap.rope().point_to_offset(Point::new(2, 9)),
            stoat_text::Bias::Right,
        )
    };
    display_map.splice_inlays(
        Vec::new(),
        vec![(hint_at, ": u32".to_string(), InlayKind::Hint)],
    );
    display_map.fold(vec![Point::new(2, 11)..Point::new(2, 24)]);
    display_map.set_wrap_width(Some(16));

    let snapshot = display_map.snapshot();
    let painted = |row: u32| -> u32 {
        snapshot
            .highlighted_chunks(row..row + 1)
            .map(|chunk| {
                chunk
                    .text
                    .chars()
                    .filter(|&ch| ch != '\n')
                    .map(display_width)
                    .sum::<u32>()
            })
            .sum()
    };

    let measured: Vec<u32> = (0..snapshot.line_count())
        .map(|row| snapshot.line_len(row))
        .collect();
    assert_eq!(
        measured,
        (0..snapshot.line_count()).map(painted).collect::<Vec<_>>(),
        "each row's measured width is the width of the cells drawn on it",
    );
}

#[test]
fn wrap_width_none_by_default() {
    let mut display_map = create_display_map("hello");
    let snapshot = display_map.snapshot();
    assert_eq!(snapshot.wrap_width(), None);
}

#[test]
fn wrap_width_after_set() {
    let mut display_map = create_display_map("hello");
    display_map.set_wrap_width(Some(40));
    let snapshot = display_map.snapshot();
    assert_eq!(snapshot.wrap_width(), Some(40));
}

/// A large edit batch hands its rewrap to the background and shows the
/// interpolated wrapping meanwhile, so long lines render unwrapped. The
/// settled result moves no buffer, fold, or inlay version, so it lands only
/// if the snapshot path re-syncs while the rewrap is outstanding.
#[test]
fn a_large_edit_settles_its_wrapping_without_another_edit() {
    let scheduler = Arc::new(TestScheduler::new());
    let executor = scheduler.executor();
    let redraw = Arc::new(tokio::sync::Notify::new());

    // Lines long enough to wrap several times each, and more than
    // WRAP_SYNC_THRESHOLD of them, so the edit takes the background path.
    let line = "the quick brown fox jumps over the lazy dog ".repeat(3);
    let pasted: String = std::iter::repeat_n(line.as_str(), 150)
        .collect::<Vec<_>>()
        .join("\n");

    let buffer = TextBuffer::with_text(BufferId::new(0), "start\n");
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared.clone());
    let mut display_map = DisplayMap::new(multi_buffer, executor.clone(), redraw.clone());
    display_map.set_wrap_width(Some(30));
    let before = display_map.snapshot().max_point().row;

    shared
        .write()
        .expect("poisoned")
        .edit(6..6, pasted.as_str());
    let interim = display_map.snapshot().max_point().row;
    assert!(
        display_map.wrap_map.background_pending(),
        "a 150-row paste must hand its rewrap to the background",
    );
    assert!(
        interim > before,
        "the interpolated snapshot still grows by the pasted rows",
    );

    scheduler.run_until_parked();
    assert!(
        notified_now(&redraw),
        "the settled rewrap must wake the run loop, since no version change will",
    );

    // No further edit arrives, so the next snapshot alone has to pick it up.
    let settled = display_map.snapshot().max_point().row;
    assert!(
        !display_map.wrap_map.background_pending(),
        "the finished rewrap must land on the next snapshot",
    );

    let mut fresh = {
        let buffer = TextBuffer::with_text(BufferId::new(0), &format!("start\n{pasted}"));
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(shared);
        DisplayMap::new(multi_buffer, executor, crate::test_notify())
    };
    fresh.set_wrap_width(Some(30));
    assert_eq!(
        settled,
        // Settled, since a file this size defers its first wrap as well.
        settled_rows(&scheduler, &mut fresh),
        "the settled wrapping must match a from-scratch build",
    );
    assert!(
        settled > interim,
        "wrapping the pasted long lines adds rows the interpolation lacked",
    );
}

/// A display map over `lines` copies of a line long enough to wrap, plus
/// the scheduler driving its background work.
fn wrappable_display_map(lines: usize) -> (Arc<TestScheduler>, DisplayMap) {
    let scheduler = Arc::new(TestScheduler::new());
    let executor = scheduler.executor();
    let line = "the quick brown fox jumps over the lazy dog ".repeat(3);
    let text: String = std::iter::repeat_n(line.as_str(), lines)
        .collect::<Vec<_>>()
        .join("\n");
    let buffer = TextBuffer::with_text(BufferId::new(0), &text);
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared);
    (
        scheduler,
        DisplayMap::new(multi_buffer, executor, crate::test_notify()),
    )
}

/// Rows the same content wraps to at `width`, built from nothing.
///
/// Settled rather than taken straight from the first snapshot, since a file
/// this size hands its first wrap to the background too.
fn rows_at_width(lines: usize, width: u32) -> u32 {
    let (scheduler, mut display_map) = wrappable_display_map(lines);
    display_map.set_wrap_width(Some(width));
    settled_rows(&scheduler, &mut display_map)
}

/// The row count once no rewrap is outstanding.
///
/// Successive rewraps chain through the pending queue, so this drains and
/// re-syncs rather than assuming one pass.
fn settled_rows(scheduler: &Arc<TestScheduler>, display_map: &mut DisplayMap) -> u32 {
    for _ in 0..10 {
        scheduler.run_until_parked();
        display_map.snapshot();
        if !display_map.wrap_map.background_pending() {
            break;
        }
    }
    assert!(
        !display_map.wrap_map.background_pending(),
        "the wrapping never settled"
    );

    display_map.snapshot().max_point().row
}

/// The first width an editor gets arrives as a change from none, and that
/// used to be the one case wrapping ran inline. Opening a large file
/// therefore walked every character of it on the run loop before anything
/// could paint. It now shows the file unwrapped for a beat instead.
#[test]
fn a_large_files_first_wrap_goes_to_the_background() {
    const LINES: usize = 150;
    let (scheduler, mut display_map) = wrappable_display_map(LINES);
    let unwrapped = display_map.snapshot().max_point().row;

    display_map.set_wrap_width(Some(20));
    let first = display_map.snapshot().max_point().row;

    assert!(
        display_map.wrap_map.background_pending(),
        "a large file's first wrap must not run on the UI thread",
    );
    assert_eq!(
        first, unwrapped,
        "so the frame painted meanwhile is the unwrapped one",
    );

    assert_eq!(
        settled_rows(&scheduler, &mut display_map),
        rows_at_width(LINES, 20),
        "and the background result lands wrapped",
    );
}

/// A short file's walk is over before a frame is due, so showing it
/// unwrapped for a beat would be the worse trade and it still arrives
/// wrapped.
///
/// Two things keep it there, either of which suffices: the row threshold
/// refuses to defer, and `flush_edits` runs a small enough batch inline
/// even when something did. What the file shows is the same either way,
/// which is what this pins.
#[test]
fn a_small_files_first_wrap_still_happens_on_the_spot() {
    const LINES: usize = 20;
    let (_scheduler, mut display_map) = wrappable_display_map(LINES);
    let unwrapped = display_map.snapshot().max_point().row;

    display_map.set_wrap_width(Some(20));
    let first = display_map.snapshot().max_point().row;

    assert!(
        !display_map.wrap_map.background_pending(),
        "a small file's first wrap has nothing to defer",
    );
    assert!(
        first > unwrapped,
        "and its first frame is already wrapped, at {first} rows from {unwrapped}",
    );
}

/// Dragging a pane edge emits a width change per resize event, and
/// rewrapping a large file is the same O(file) walk a large edit batch
/// already defers, so it must not run on the UI thread either.
#[test]
fn a_wide_files_width_change_rewraps_in_the_background() {
    const LINES: usize = 150;
    let (scheduler, mut display_map) = wrappable_display_map(LINES);
    // Settled first, so the width change below is the subject rather than
    // the first wrap this file also defers.
    display_map.set_wrap_width(Some(60));
    let at_60 = settled_rows(&scheduler, &mut display_map);

    display_map.set_wrap_width(Some(20));
    let interim = display_map.snapshot().max_point().row;
    assert!(
        display_map.wrap_map.background_pending(),
        "a large file's width change must not rewrap on the UI thread",
    );
    assert_eq!(
        interim, at_60,
        "the immediate snapshot still carries the previous width's wrapping",
    );

    scheduler.run_until_parked();
    let settled = display_map.snapshot().max_point().row;
    assert!(
        !display_map.wrap_map.background_pending(),
        "the finished rewrap lands on the next snapshot",
    );
    assert_eq!(
        settled,
        rows_at_width(LINES, 20),
        "the settled wrapping matches a from-scratch build at the new width",
    );
    assert!(settled > at_60, "a narrower width wraps into more rows");
}

/// Below the threshold the inline rebuild is cheaper than a task, so the
/// new width has to be on screen the moment it is set.
#[test]
fn a_small_files_width_change_rewraps_synchronously() {
    const LINES: usize = 4;
    let (_scheduler, mut display_map) = wrappable_display_map(LINES);
    display_map.set_wrap_width(Some(60));
    display_map.snapshot();

    display_map.set_wrap_width(Some(20));
    let rows = display_map.snapshot().max_point().row;
    assert!(
        !display_map.wrap_map.background_pending(),
        "a small file rewraps inline, leaving nothing outstanding",
    );
    assert_eq!(
        rows,
        rows_at_width(LINES, 20),
        "and the new width is already in the first snapshot after the change",
    );
}

/// A drag emits many widths in a row. Each intermediate one may be
/// abandoned, but the last must be what the display settles on.
#[test]
fn successive_width_changes_settle_on_the_last() {
    const LINES: usize = 150;
    let (scheduler, mut display_map) = wrappable_display_map(LINES);
    display_map.set_wrap_width(Some(60));
    display_map.snapshot();

    for width in [50, 40, 30, 20] {
        display_map.set_wrap_width(Some(width));
        display_map.snapshot();
    }

    // Successive rewraps chain through the pending queue, so drain and
    // re-sync until nothing is outstanding rather than assuming one pass.
    for _ in 0..10 {
        scheduler.run_until_parked();
        display_map.snapshot();
        if !display_map.wrap_map.background_pending() {
            break;
        }
    }

    assert!(
        !display_map.wrap_map.background_pending(),
        "the chain of width changes must converge",
    );
    assert_eq!(
        display_map.snapshot().max_point().row,
        rows_at_width(LINES, 20),
        "the display settles on the last width, not an abandoned one",
    );
}

/// Whether `notify` is holding a permit, without awaiting one.
///
/// `notify_one` stores a permit when nobody is waiting, so a `notified()`
/// future polled afterwards completes at once. Polling it a single time is
/// how a synchronous test observes that the wake happened.
fn notified_now(notify: &Arc<tokio::sync::Notify>) -> bool {
    let mut fut = Box::pin(notify.notified());
    let waker = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    fut.as_mut().poll(&mut cx).is_ready()
}

#[test]
fn a_highlight_change_keeps_the_layers_it_did_not_touch() {
    // A parse installs a token channel on every keystroke in a highlighted
    // file, so this is the difference between a typed character paying the
    // five-layer pipeline once or twice.
    let shared = Arc::new(RwLock::new(TextBuffer::with_text(
        BufferId::new(0),
        "fn alpha() {}\nfn beta() {}\n",
    )));
    let mut display_map = DisplayMap::new(
        MultiBuffer::singleton(shared.clone()),
        test_executor(),
        crate::test_notify(),
    );

    let (tokens, interner) = {
        let snap = display_map.multi_buffer.snapshot();
        let mut interner = HighlightStyleInterner::default();
        let style = interner.push(HighlightStyle::default());
        let tokens: Arc<[SemanticTokenHighlight]> = [(3usize, 8usize)]
            .iter()
            .map(|&(start, end)| SemanticTokenHighlight {
                range: snap.anchor_at(start, stoat_text::Bias::Right)
                    ..snap.anchor_at(end, stoat_text::Bias::Left),
                style,
            })
            .collect();
        (tokens, Arc::new(interner))
    };

    // The block snapshot derefs through an Arc, so the address of what it
    // points at says whether the layers below were rebuilt or handed over.
    let layers = |snapshot: &DisplaySnapshot| std::ptr::from_ref(&*snapshot.block_snapshot);

    let before = display_map.snapshot();
    assert!(
        display_map
            .semantic_token_highlights
            .get(&BufferId::new(0))
            .is_none(),
        "nothing is installed yet",
    );

    display_map.set_semantic_token_highlights(BufferId::new(0), tokens, interner);
    let after = display_map.snapshot();

    assert_eq!(
        layers(&before),
        layers(&after),
        "no edit happened, so the layers are the same ones",
    );
    assert!(
        after
            .semantic_token_highlights
            .get(&BufferId::new(0))
            .is_some(),
        "and the snapshot carries the highlights that were installed",
    );
}

#[test]
fn an_unfold_that_removes_nothing_costs_no_wrap_rebuild() {
    // What the version bump actually buys downstream. A fold-version change
    // with no text edit alongside is WrapMap::sync's whole-file rebuild
    // condition, and the whole-range patch it emits invalidates the block
    // map and every render cache below it.
    let mut display_map = create_display_map("line0\nline1\nline2\nline3");
    let _ = display_map.snapshot();

    display_map.unfold(vec![Point::new(0, 0)..Point::new(3, 5)]);
    let wrap_edits = display_map.sync_through_wrap().wrap_edits;
    assert!(
        wrap_edits.is_empty(),
        "no fold was there to remove: {:?}",
        wrap_edits.edits(),
    );

    // The same call after a fold that does land reports the rebuild, so the
    // emptiness above is the unfold being a no-op rather than this sync
    // never reporting anything.
    display_map.fold(vec![Point::new(2, 0)..Point::new(2, 5)]);
    let wrap_edits = display_map.sync_through_wrap().wrap_edits;
    assert!(!wrap_edits.is_empty(), "a fold that lands rebuilds");
}

#[test]
fn is_line_folded_through_display() {
    let mut display_map = create_display_map("line0\nline1\nline2\nline3");
    display_map.fold(vec![Point::new(1, 0)..Point::new(2, 5)]);
    let snapshot = display_map.snapshot();
    assert!(!snapshot.is_line_folded(0));
    assert!(snapshot.is_line_folded(1));
    assert!(snapshot.is_line_folded(2));
    assert!(!snapshot.is_line_folded(3));
}

#[test]
fn buffer_chars_at_simple() {
    let mut display_map = create_display_map("hello");
    let snapshot = display_map.snapshot();
    let chars: Vec<(char, Point)> = snapshot.buffer_chars_at(Point::new(0, 0)).collect();
    assert_eq!(
        chars,
        vec![
            ('h', Point::new(0, 0)),
            ('e', Point::new(0, 1)),
            ('l', Point::new(0, 2)),
            ('l', Point::new(0, 3)),
            ('o', Point::new(0, 4)),
        ]
    );
}

#[test]
fn buffer_chars_at_multiline() {
    let mut display_map = create_display_map("ab\ncd");
    let snapshot = display_map.snapshot();
    let chars: Vec<(char, Point)> = snapshot.buffer_chars_at(Point::new(0, 0)).collect();
    assert_eq!(
        chars,
        vec![
            ('a', Point::new(0, 0)),
            ('b', Point::new(0, 1)),
            ('\n', Point::new(0, 2)),
            ('c', Point::new(1, 0)),
            ('d', Point::new(1, 1)),
        ]
    );
}

#[test]
fn reverse_buffer_chars_at_simple() {
    let mut display_map = create_display_map("hello");
    let snapshot = display_map.snapshot();
    let chars: Vec<(char, Point)> = snapshot.reverse_buffer_chars_at(Point::new(0, 5)).collect();
    assert_eq!(
        chars,
        vec![
            ('o', Point::new(0, 4)),
            ('l', Point::new(0, 3)),
            ('l', Point::new(0, 2)),
            ('e', Point::new(0, 1)),
            ('h', Point::new(0, 0)),
        ]
    );
}

#[test]
fn reverse_buffer_chars_at_multiline() {
    let mut display_map = create_display_map("ab\ncd");
    let snapshot = display_map.snapshot();
    let chars: Vec<(char, Point)> = snapshot.reverse_buffer_chars_at(Point::new(1, 2)).collect();
    assert_eq!(
        chars,
        vec![
            ('d', Point::new(1, 1)),
            ('c', Point::new(1, 0)),
            ('\n', Point::new(0, 2)),
            ('b', Point::new(0, 1)),
            ('a', Point::new(0, 0)),
        ]
    );
}

#[test]
fn prev_line_boundary_test() {
    let mut display_map = create_display_map("hello\nworld");
    let snapshot = display_map.snapshot();
    let (buf, display) = snapshot.prev_line_boundary(Point::new(1, 3));
    assert_eq!(buf, Point::new(1, 0));
    assert_eq!(display, DisplayPoint::new(1, 0));
}

#[test]
fn next_line_boundary_test() {
    let mut display_map = create_display_map("hello\nworld");
    let snapshot = display_map.snapshot();
    let (buf, display) = snapshot.next_line_boundary(Point::new(0, 2));
    assert_eq!(buf, Point::new(0, 5));
    assert_eq!(display, DisplayPoint::new(0, 5));
}

#[test]
fn clip_at_line_end_test() {
    let mut display_map = create_display_map("hello\nhi");
    let snapshot = display_map.snapshot();
    let clipped = snapshot.clip_at_line_end(DisplayPoint::new(0, 100));
    assert_eq!(clipped, DisplayPoint::new(0, 5));
}

#[test]
fn inlay_survives_compaction() {
    let buffer = TextBuffer::with_text(BufferId::new(0), "hello world");
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared.clone());
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

    let snap = display_map.multi_buffer.snapshot();
    let off = snap.rope().point_to_offset(Point::new(0, 5));
    let anchor = snap.anchor_at(off, stoat_text::Bias::Right);
    display_map.inlay_map.splice(
        &snap,
        Vec::new(),
        vec![(anchor, ": str".to_string(), InlayKind::Hint)],
    );

    for i in 0..10 {
        {
            let mut buf = shared.write().unwrap();
            let prefix = format!("{i}");
            buf.edit(0..0, &prefix);
        }
        let _ = display_map.snapshot();
    }

    let snapshot = display_map.snapshot();
    let inlay_snap = snapshot.inlay_snapshot();
    assert_eq!(
        inlay_snap.to_inlay_point(Point::new(0, 15)),
        InlayPoint::new(0, 20)
    );
}

#[test]
fn mid_line_inlay_keeps_trailing_buffer_text() {
    let buffer = TextBuffer::with_text(BufferId::new(0), "let x = 1\n");
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared);
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());

    let anchor = {
        let snap = display_map.multi_buffer.snapshot();
        let off = snap.rope().point_to_offset(Point::new(0, 5));
        snap.anchor_at(off, stoat_text::Bias::Left)
    };
    {
        let snap = display_map.multi_buffer.snapshot();
        display_map.inlay_map.splice(
            &snap,
            Vec::new(),
            vec![(anchor, ": u32".to_string(), InlayKind::Hint)],
        );
    }

    let snapshot = display_map.snapshot();
    let chunks: Vec<_> = snapshot.highlighted_chunks(0..1).collect();
    let text: String = chunks.iter().map(|c| c.text.as_ref()).collect();
    assert_eq!(text, "let x: u32 = 1");
}

/// The rows a splice patches have to arrive intact at the far end of the
/// pipeline, where the fold, wrap, and block layers each sync against the
/// inlay patch rather than rebuilding. A hint landing deep in a file is
/// where a mis-scoped patch would leave the wrong row painted.
#[test]
fn a_spliced_hint_paints_through_every_layer() {
    // A fixed-width name puts the hint's column at the same place on every
    // row, so the assertions below name one column rather than three.
    let text: String = (0..200).map(|i| format!("let x{i:03} = {i}\n")).collect();
    let buffer = TextBuffer::with_text(BufferId::new(0), &text);
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared);
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
    display_map.snapshot();

    let hint_at = |display_map: &DisplayMap, row: u32| {
        let snap = display_map.multi_buffer.snapshot();
        let off = snap.rope().point_to_offset(Point::new(row, 8));
        snap.anchor_at(off, stoat_text::Bias::Left)
    };

    let row_text = |display_map: &mut DisplayMap, row: u32| -> String {
        let snapshot = display_map.snapshot();
        snapshot
            .highlighted_chunks(row..row + 1)
            .map(|chunk| chunk.text.to_string())
            .collect()
    };

    let anchor = hint_at(&display_map, 150);
    let ids = display_map.splice_inlays(
        Vec::new(),
        vec![(anchor, ": u32".to_string(), InlayKind::Hint)],
    );
    assert_eq!(row_text(&mut display_map, 150), "let x150: u32 = 150");
    assert_eq!(row_text(&mut display_map, 149), "let x149 = 149");

    // Replacing the set is what an unchanged hint refresh looks like, and it
    // has to leave the same text behind.
    let anchor = hint_at(&display_map, 150);
    display_map.splice_inlays(ids, vec![(anchor, ": u32".to_string(), InlayKind::Hint)]);
    assert_eq!(row_text(&mut display_map, 150), "let x150: u32 = 150");
}

/// A splice resolves its offsets against the buffer as it stands, and the
/// inlay layer can only place them while its own text still reads the same.
/// An edit arriving first strands them, and the layer falls back to marking
/// every row for rebuild, which each layer downstream then repeats.
#[test]
fn an_edit_after_a_splice_rebuilds_only_the_rows_it_touched() {
    let text: String = (0..200).map(|i| format!("let x{i:03} = {i}\n")).collect();
    let buffer = TextBuffer::with_text(BufferId::new(0), &text);
    let shared = Arc::new(RwLock::new(buffer));
    let multi_buffer = MultiBuffer::singleton(shared.clone());
    let mut display_map = DisplayMap::new(multi_buffer, test_executor(), crate::test_notify());
    display_map.snapshot();

    let anchor = {
        let snap = display_map.multi_buffer.snapshot();
        let offset = snap.rope().point_to_offset(Point::new(150, 8));
        snap.anchor_at(offset, stoat_text::Bias::Left)
    };
    display_map.splice_inlays(
        Vec::new(),
        vec![(anchor, ": u32".to_string(), InlayKind::Hint)],
    );

    shared.write().expect("poisoned").edit(0..0, "x");

    let WrapSync {
        snapshot: wrap_snapshot,
        wrap_edits,
        ..
    } = display_map.sync_through_wrap();
    assert!(!wrap_edits.is_empty(), "the edit itself must be reported");
    assert!(
        !wrap_edits
            .edits()
            .iter()
            .any(|edit| edit.new.contains(&150)),
        "an edit on the first row must not rebuild row 150: {:?}",
        wrap_edits.edits(),
    );
    let painted: String = wrap_snapshot
        .chunks(150..151, Arc::from(Vec::new()))
        .map(|chunk| chunk.text.to_string())
        .collect();
    assert_eq!(
        painted, "let x150: u32 = 150",
        "the spliced hint survives the edit",
    );
}

#[test]
fn a_viewport_in_a_long_line_resolves_only_the_rows_it_shows() {
    use super::highlights::{HighlightKey, HighlightLayer, HighlightStyle};
    use stoat_text::Bias;

    const TOKEN_STRIDE: usize = 6;
    const WRAP_WIDTH: u32 = 10;
    const ROWS: u32 = 40;

    let text = "ab cd ".repeat(4000);
    let mut display_map = create_display_map(&text);
    display_map.set_wrap_width(Some(WRAP_WIDTH));

    let ranges = {
        let snap = display_map.multi_buffer.snapshot();
        (0..text.len() / TOKEN_STRIDE)
            .map(|i| {
                let start = i * TOKEN_STRIDE;
                snap.anchor_at(start, Bias::Right)..snap.anchor_at(start + 2, Bias::Left)
            })
            .collect()
    };
    let key = HighlightKey::layer(HighlightLayer::DocumentHighlightRead);
    display_map.highlight_text(key, ranges, HighlightStyle::default());

    let snapshot = display_map.snapshot();
    assert!(
        snapshot.line_count() > ROWS * 4,
        "the line wraps into far more rows than the window shows",
    );

    // Two endpoints per token, and a window of ROWS rows at WRAP_WIDTH
    // columns shows at most ROWS * WRAP_WIDTH bytes of this ASCII text.
    let cap = 2 * (ROWS as usize * WRAP_WIDTH as usize / TOKEN_STRIDE + 2);
    let endpoints = snapshot.highlighted_endpoints(0..ROWS);

    assert!(
        endpoints.len() <= cap,
        "a window over {ROWS} rows resolved {} endpoints, over a cap of {cap}",
        endpoints.len(),
    );
    assert!(
        !endpoints.is_empty(),
        "the tokens the window does show still resolve",
    );
}

#[test]
fn soft_wrap_indent_exposed() {
    let mut display_map = create_display_map("    hello world foo");
    display_map.set_wrap_width(Some(8));
    let snapshot = display_map.snapshot();
    assert_eq!(snapshot.soft_wrap_indent(0), 0);
    if snapshot.line_count() > 1 {
        assert_eq!(snapshot.soft_wrap_indent(1), 4);
    }
}

#[test]
fn display_lines_empty_range() {
    let mut display_map = create_display_map("hello\nworld");
    let snapshot = display_map.snapshot();
    let lines: Vec<String> = snapshot.display_lines(0..0).collect();
    assert!(lines.is_empty());
}

#[test]
fn display_lines_multi_line() {
    let mut display_map = create_display_map("hello\nworld\nfoo");
    let snapshot = display_map.snapshot();
    let lines: Vec<String> = snapshot.display_lines(0..3).collect();
    assert_eq!(lines, vec!["hello", "world", "foo"]);
}

#[test]
fn cjk_wide_chars_display_width() {
    let mut display_map = create_display_map("ab\u{4f60}\u{597d}cd");
    let snapshot = display_map.snapshot();
    // "ab" = 2, "你" = 2, "好" = 2, "cd" = 2 => total 8
    assert_eq!(snapshot.line_len(0), 8);
}

#[test]
fn cjk_wrap_at_correct_column() {
    let mut display_map = create_display_map("ab\u{4f60}\u{597d}cd");
    display_map.set_wrap_width(Some(5));
    let snapshot = display_map.snapshot();
    // "ab你" = 4 cols, "好cd" = 4 cols -> wraps after 你
    assert_eq!(snapshot.line_count(), 2);
}

#[test]
fn a_goal_column_walk_stops_at_a_multi_byte_rows_end() {
    use stoat_text::Bias;
    let mut display_map = create_display_map("aaaaaaaaaa\n\u{4f60}\ncccccccc");
    let snapshot = display_map.snapshot();

    for bias in [Bias::Left, Bias::Right] {
        assert_eq!(
            snapshot.buffer_column_at_visual(1, 10, bias),
            3,
            "a goal column past the row's end answers the row's own length, {bias:?}"
        );
    }
    assert_eq!(
        snapshot.visual_column(Point::new(1, 3)),
        2,
        "the row's one character spans three bytes and two cells"
    );
}

#[test]
fn write_display_line_matches_display_line() {
    let base = "line1\ndeleted\nline2";
    let diff = make_diff_with_deletion(0, base, 6..13, 1);
    let mut display_map = create_display_map_with_diff("line1\nline2", diff);
    let snapshot = display_map.snapshot();
    for row in 0..snapshot.line_count() {
        let expected = snapshot.display_line(row);
        let mut buf = String::new();
        snapshot.write_display_line(&mut buf, row);
        assert_eq!(buf, expected, "mismatch at row {row}");
    }
}

#[test]
fn chunks_match_display_lines() {
    let mut display_map = create_display_map("hello\nworld\nfoo bar");
    let snapshot = display_map.snapshot();
    let total = snapshot.line_count();

    let chunks: Vec<_> = snapshot.highlighted_chunks(0..total).collect();
    let from_chunks: String = chunks.iter().map(|c| c.text.as_ref()).collect();
    let from_lines: String = (0..total)
        .map(|r| snapshot.display_line(r))
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(from_chunks, from_lines);
}

#[test]
fn chunks_with_blocks_match_display_lines() {
    let diff = DiffMap::from_hunks(
        [DiffHunk {
            status: DiffHunkStatus::Deleted,
            unstaged_lines: std::iter::once(2..2).collect(),
            marked_rows: Vec::new(),
            buffer_start_line: 2,
            buffer_line_range: 2..2,
            base_byte_range: 0..7,
            anchor_range: None,
            token_detail: None,
        }],
        Some(Arc::new("deleted".to_string())),
    );
    let mut display_map = create_display_map_with_diff("aaa\nbbb\nccc", diff);
    let snapshot = display_map.snapshot();
    let total = snapshot.line_count();

    let chunks: Vec<_> = snapshot.highlighted_chunks(0..total).collect();
    let from_chunks: String = chunks.iter().map(|c| c.text.as_ref()).collect();
    let from_lines: String = (0..total)
        .map(|r| snapshot.display_line(r))
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(from_chunks, from_lines);
}

#[test]
fn snapshot_open_rust_file_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("sample.rs", "fn main() {\n    let x = \"hi\";\n}\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_rust_file_highlights");
}

#[test]
fn snapshot_open_json_file_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("sample.json", "{\n  \"a\": 1\n}\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_json_file_highlights");
}

#[test]
fn snapshot_open_markdown_file_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("sample.md", "# Title\n\nbody\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_markdown_file_highlights");
}

#[test]
fn snapshot_open_markdown_file_with_bold_inline() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("bold.md", "# Title\n\n**bold** text\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_markdown_file_with_bold_inline");
}

#[test]
fn snapshot_open_unknown_extension_no_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("sample.txt", "fn main() {}\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_unknown_extension_no_highlights");
}

#[test]
fn snapshot_open_rust_file_nested_captures() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("nested.rs", "fn main() { \"a\\nb\"; }\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_rust_file_nested_captures");
}

#[test]
fn snapshot_open_rust_file_then_edit_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("edit.rs", "fn a() {}\n");

    h.open_file(&path);
    h.edit_focused(8..8, " let x = 1; ");
    h.assert_snapshot("snapshot_open_rust_file_then_edit_highlights");
}

#[test]
fn snapshot_rust_doc_comment_markdown() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file(
        "doc.rs",
        "/// A **bold** word and a [link](url).\nfn a() {}\n",
    );

    h.open_file(&path);
    h.assert_snapshot("snapshot_rust_doc_comment_markdown");
}

#[test]
fn snapshot_markdown_fence_highlight() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 8);
    let path = h.write_file("doc.md", "# Title\n\n```rust\nfn a() {}\n```\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_markdown_fence_highlight");
}

#[test]
fn snapshot_open_rust_file_with_fold() {
    use stoat_text::Point;
    let mut h = crate::test_harness::TestHarness::with_size(40, 8);
    let path = h.write_file("folded.rs", "fn a() { 1 }\nfn b() { 2 }\nfn c() { 3 }\n");

    h.open_file(&path);
    h.fold_focused(Point::new(1, 7)..Point::new(1, 12));
    h.assert_snapshot("snapshot_open_rust_file_with_fold");
}

/// Swapping the interner reaches the painted chunks, not just the stored
/// channels.
///
/// Endpoints bake a resolved style and are cached across frames, so a swap
/// that updated the channels without invalidating that cache would leave
/// the old colors on screen. This drives the same cache across the swap.
#[test]
fn swapping_the_interner_repaints_through_the_endpoint_cache() {
    use crate::display_map::highlights::{
        HighlightStyle, HighlightStyleInterner, SemanticTokenHighlight,
    };
    use ratatui::style::Color;

    let interner_with = |color: Color| {
        let mut interner = HighlightStyleInterner::default();
        interner.push(HighlightStyle {
            foreground: Some(color),
            ..Default::default()
        });
        Arc::new(interner)
    };

    let mut display_map = create_display_map("let x = 1\n");
    let style_id = {
        let mut probe = HighlightStyleInterner::default();
        probe.push(HighlightStyle::default())
    };
    let token = {
        let snap = display_map.multi_buffer.snapshot();
        let start = snap.anchor_at(0, stoat_text::Bias::Right);
        let end = snap.anchor_at(3, stoat_text::Bias::Left);
        SemanticTokenHighlight {
            range: start..end,
            style: style_id,
        }
    };
    display_map.set_semantic_token_highlights(
        BufferId::new(0),
        Arc::from(vec![token]),
        interner_with(Color::Red),
    );

    let mut cache = None;
    let painted = |display_map: &mut DisplayMap, cache: &mut Option<_>| {
        let snapshot = display_map.snapshot();
        snapshot
            .highlighted_chunks_cached(0..1, cache)
            .filter_map(|chunk| chunk.highlight_style?.foreground)
            .collect::<Vec<_>>()
    };

    assert_eq!(painted(&mut display_map, &mut cache), vec![Color::Red]);

    display_map.swap_style_interner(&interner_with(Color::Blue));
    assert_eq!(
        painted(&mut display_map, &mut cache),
        vec![Color::Blue],
        "the cached endpoints are rebuilt against the new interner"
    );
}

/// A token install that finds no second reference to the channel map
/// changes no pointer, so a pointer check alone never sees it.
///
/// The second reference is the map's own cached snapshot, and several
/// setters drop it without moving any version. An install landing in that
/// window mutates the channel map in place, leaving every pointer the cache
/// compares equal to what it stored, so the fresh tokens go unpainted until
/// an edit or a scroll moves the buffer version or the visible range.
#[test]
fn a_token_install_holding_the_only_reference_still_repaints() {
    use crate::display_map::highlights::{
        HighlightStyle, HighlightStyleInterner, SemanticTokenHighlight,
    };
    use ratatui::style::Color;

    let interner_with = |color: Color| {
        let mut interner = HighlightStyleInterner::default();
        interner.push(HighlightStyle {
            foreground: Some(color),
            ..Default::default()
        });
        Arc::new(interner)
    };

    let mut display_map = create_display_map("let x = 1\n");
    let style_id = {
        let mut probe = HighlightStyleInterner::default();
        probe.push(HighlightStyle::default())
    };
    let token = {
        let snap = display_map.multi_buffer.snapshot();
        SemanticTokenHighlight {
            range: snap.anchor_at(0, stoat_text::Bias::Right)
                ..snap.anchor_at(3, stoat_text::Bias::Left),
            style: style_id,
        }
    };
    let install = |display_map: &mut DisplayMap, color: Color| {
        display_map.set_semantic_token_highlights(
            BufferId::new(0),
            Arc::from(vec![token.clone()]),
            interner_with(color),
        );
    };

    let mut cache = None;
    let painted = |display_map: &mut DisplayMap, cache: &mut Option<_>| {
        let snapshot = display_map.snapshot();
        snapshot
            .highlighted_chunks_cached(0..1, cache)
            .filter_map(|chunk| chunk.highlight_style?.foreground)
            .collect::<Vec<_>>()
    };

    install(&mut display_map, Color::Red);
    assert_eq!(painted(&mut display_map, &mut cache), vec![Color::Red]);

    // Drops the cached snapshot and moves no version, which is what leaves
    // the install below holding the only reference to the channel map.
    display_map.insert_blocks(Vec::new());
    install(&mut display_map, Color::Blue);

    assert_eq!(
        painted(&mut display_map, &mut cache),
        vec![Color::Blue],
        "the install is newer than the cached endpoints however the Arc behaved"
    );
}

#[test]
fn clearing_an_absent_highlight_key_leaves_the_map_alone() {
    use super::highlights::{HighlightKey, HighlightLayer, HighlightStyle};
    use stoat_text::Bias;

    let mut display_map = create_display_map("fn alpha() {}\n");
    let present = HighlightKey::layer(HighlightLayer::DocumentHighlightRead);
    let absent = HighlightKey::layer(HighlightLayer::DocumentHighlightWrite);

    let range = {
        let snap = display_map.multi_buffer.snapshot();
        snap.anchor_at(3, Bias::Right)..snap.anchor_at(8, Bias::Left)
    };
    display_map.highlight_text(present, vec![range], HighlightStyle::default());

    // A live snapshot is what puts the map's refcount above one, which is
    // the condition under which a mutable borrow would deep-clone it.
    let _snapshot = display_map.snapshot();
    display_map.highlights_dirty = false;
    let before = display_map.text_highlights.clone();

    assert!(
        !display_map.clear_highlights(absent),
        "nothing was stored under the absent key",
    );
    assert!(
        Arc::ptr_eq(&before, &display_map.text_highlights),
        "clearing an absent key must not rebuild the map",
    );
    assert!(
        !display_map.highlights_dirty,
        "a clear that removed nothing leaves the highlights clean",
    );

    assert!(
        display_map.clear_highlights(present),
        "the stored key still clears",
    );
    assert!(display_map.highlights_dirty, "a real clear marks dirty");
}

/// A keystroke takes six to eight snapshots, and each one used to walk and
/// reallocate the whole nested inlay-highlight map. Sharing it makes a
/// snapshot a refcount bump, and a highlight change still takes its own
/// copy so no snapshot already handed out sees the mutation.
#[test]
fn snapshots_share_the_inlay_highlight_map_until_it_changes() {
    use super::highlights::{HighlightKey, HighlightLayer, HighlightStyle, InlayHighlight};
    use stoat_text::Bias;

    let mut display_map = create_display_map("fn alpha() {}\n");
    let anchor = display_map
        .multi_buffer
        .snapshot()
        .anchor_at(3, Bias::Right);
    let ids = display_map.splice_inlays(
        Vec::new(),
        vec![(anchor, ": u32".to_string(), InlayKind::Hint)],
    );
    let inlay = *ids.first().expect("the splice added an inlay");

    let key = HighlightKey::layer(HighlightLayer::DocumentHighlightRead);
    display_map.highlight_inlays(
        key,
        vec![InlayHighlight { inlay, range: 0..5 }],
        HighlightStyle::default(),
    );

    let first = display_map.snapshot();
    assert_eq!(first.inlay_highlights().len(), 1, "the fixture stored one");
    assert!(
        Arc::ptr_eq(
            first.inlay_highlights(),
            display_map.snapshot().inlay_highlights()
        ),
        "a snapshot served from the cache shares the map rather than copying it",
    );

    // Splicing an inlay invalidates the cache without touching a highlight,
    // so the rebuild has to carry the same map forward rather than mint one.
    display_map.splice_inlays(
        Vec::new(),
        vec![(anchor, ": u8".to_string(), InlayKind::Hint)],
    );
    assert!(
        Arc::ptr_eq(
            first.inlay_highlights(),
            display_map.snapshot().inlay_highlights()
        ),
        "a rebuild that changed no highlight shares the map too",
    );

    assert!(display_map.clear_highlights(key), "the key was stored");
    let third = display_map.snapshot();
    assert!(
        !Arc::ptr_eq(first.inlay_highlights(), third.inlay_highlights()),
        "a highlight change copies rather than mutating a shared map",
    );
    assert_eq!(
        first.inlay_highlights().len(),
        1,
        "so a snapshot taken before the clear still sees the highlight",
    );
    assert!(
        third.inlay_highlights().is_empty(),
        "and one taken after sees it gone",
    );
}

#[test]
fn highlight_text_orders_ranges_by_resolved_start() {
    use super::highlights::{HighlightKey, HighlightLayer, HighlightStyle};
    use stoat_text::{Anchor, Bias};

    let shared = Arc::new(RwLock::new(TextBuffer::with_text(
        BufferId::new(0),
        "aaa\nbbb\nccc\nddd\n",
    )));
    let mut display_map = DisplayMap::new(
        MultiBuffer::singleton(shared.clone()),
        test_executor(),
        crate::test_notify(),
    );

    let ranges: Vec<Range<Anchor>> = {
        let snap = display_map.multi_buffer.snapshot();
        [(12usize, 15usize), (0, 3), (8, 11), (4, 7)]
            .iter()
            .map(|&(start, end)| {
                snap.anchor_at(start, Bias::Right)..snap.anchor_at(end, Bias::Left)
            })
            .collect()
    };

    // Edited after the anchors were minted, so their stored offsets are no
    // longer where they resolve and the sort has to ask the buffer.
    {
        let mut buf = shared.write().expect("poisoned");
        buf.edit(0..0, "// header\n");
    }

    let key = HighlightKey::layer(HighlightLayer::DocumentHighlightRead);
    display_map.highlight_text(key, ranges, HighlightStyle::default());

    let snapshot = display_map.multi_buffer.snapshot();
    let stored = display_map
        .text_highlights
        .get(&key)
        .expect("the ranges are stored under their key");
    let starts: Vec<usize> = stored
        .1
        .iter()
        .map(|range| snapshot.resolve_anchor(&range.start))
        .collect();

    assert_eq!(
        starts,
        vec![10, 14, 18, 22],
        "ranges are stored ascending by resolved start, whatever order they arrived in",
    );
}

/// Painting row by row through a carried replay renders each row exactly as
/// opening that row's stream on its own does.
///
/// The replay is only a shortcut past work the per-row stream would repeat,
/// so any divergence is a colouring bug, and one that paints plausible text
/// rather than failing. Tokens overlap across rows so a row's opening styles
/// genuinely depend on what the rows above it left active, and the check
/// runs wrapped as well as unwrapped because the two take different paths
/// through the wrap layer.
#[test]
fn row_by_row_painting_matches_opening_each_row_alone() {
    use super::highlights::{HighlightStyle, HighlightStyleInterner, SemanticTokenHighlight};
    use ratatui::style::Color;
    use stoat_text::Bias;

    let text = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\nfn delta() {}\nfn epsilon() {}\n";
    let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), text)));
    let mut display_map = DisplayMap::new(
        MultiBuffer::singleton(shared.clone()),
        test_executor(),
        crate::test_notify(),
    );

    let (tokens, interner) = {
        let snap = display_map.multi_buffer.snapshot();
        let mut interner = HighlightStyleInterner::default();
        let colors = [Color::Red, Color::Green, Color::Blue].map(|color| {
            interner.push(HighlightStyle {
                foreground: Some(color),
                ..Default::default()
            })
        });
        // Spans deliberately straddle row boundaries, and start and end at
        // offsets that are not multiples of any wrap width tried below, so
        // some boundaries fall part-way through a continuation row. A row
        // opened cold and one opened from the replay agree there only if the
        // replay is right.
        let tokens: Arc<[SemanticTokenHighlight]> = [(3usize, 57usize), (11, 37), (23, 62)]
            .iter()
            .zip(colors)
            .map(|(&(start, end), style)| SemanticTokenHighlight {
                range: snap.anchor_at(start, Bias::Right)..snap.anchor_at(end, Bias::Left),
                style,
            })
            .collect();
        (tokens, Arc::new(interner))
    };
    display_map.set_semantic_token_highlights(BufferId::new(0), tokens, interner);

    for wrap_width in [None, Some(5u32), Some(8u32), Some(11u32)] {
        display_map.set_wrap_width(wrap_width);
        let snapshot = display_map.snapshot();
        let rows = 0..snapshot.line_count();
        let endpoints = snapshot.highlighted_endpoints(rows.clone());

        let styled = |chunks: super::BlockChunks<'_>| {
            chunks
                .map(|c| (c.text.into_owned(), c.highlight_style))
                .collect::<Vec<_>>()
        };
        let mut cursor = snapshot.row_highlight_cursor(endpoints.clone());
        for row in rows {
            assert_eq!(
                styled(snapshot.row_chunks(row, &mut cursor)),
                styled(snapshot.highlighted_chunks_with_endpoints(row..row + 1, endpoints.clone())),
                "row {row} diverged at wrap width {wrap_width:?}",
            );
        }
    }
}

#[test]
fn installing_tokens_indexes_them_like_a_per_anchor_build() {
    use super::highlights::{
        BufferSemanticTokens, HighlightStyle, HighlightStyleInterner, SemanticTokenHighlight,
    };
    use stoat_text::{Anchor, Bias};

    let shared = Arc::new(RwLock::new(TextBuffer::with_text(
        BufferId::new(0),
        "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n",
    )));
    let mut display_map = DisplayMap::new(
        MultiBuffer::singleton(shared.clone()),
        test_executor(),
        crate::test_notify(),
    );

    let (tokens, interner) = {
        let snap = display_map.multi_buffer.snapshot();
        let mut interner = HighlightStyleInterner::default();
        let style = interner.push(HighlightStyle::default());
        // An enclosing token ahead of the three it contains, the order
        // captures() emits an outer node and its children in. Without one
        // the ends rise monotonically, the running argmax is the identity,
        // and the index cannot distinguish a correct build from a wrong one.
        let tokens: Arc<[SemanticTokenHighlight]> = [(0usize, 40usize), (3, 8), (17, 21), (30, 35)]
            .iter()
            .map(|&(start, end)| SemanticTokenHighlight {
                range: snap.anchor_at(start, Bias::Right)..snap.anchor_at(end, Bias::Left),
                style,
            })
            .collect();
        (tokens, Arc::new(interner))
    };

    // Fragment the buffer first. On an untouched one every anchor still
    // resolves to the offset it was minted from, so any way of building the
    // index agrees and the comparison below proves nothing.
    {
        let mut buf = shared.write().expect("poisoned");
        buf.edit(0..0, "// header\n");
        buf.edit(20..20, "  ");
    }

    display_map.set_semantic_token_highlights(BufferId::new(0), tokens.clone(), interner.clone());

    let snapshot = display_map.multi_buffer.snapshot();
    let resolve = |a: &Anchor| snapshot.resolve_anchor(a);
    let per_anchor = BufferSemanticTokens::new(tokens, interner, resolve);
    let installed = display_map
        .semantic_token_highlights
        .get(&BufferId::new(0))
        .expect("the channel is installed under its buffer id");

    let len = snapshot.rope().len();
    for start in 0..=len {
        for end in [start, start + 1, len] {
            let end = end.min(len);
            assert_eq!(
                installed.overlap_bounds(&(start..end), resolve),
                per_anchor.overlap_bounds(&(start..end), resolve),
                "bounds must agree for {start}..{end}",
            );
        }
    }
}

/// Every row an incrementally-synced map renders matches a map built from
/// scratch over the same text, folds, blocks, inlays and wrap width.
///
/// Each layer hands the next a row patch saying which rows changed and how
/// many replaced them. A patch that misreports either sends every layer
/// above it building a tree over rows that moved, and the result is a
/// rendered row in the wrong place rather than a crash or a bad count. The
/// other randomized display-map tests read carried fold offsets and fold
/// records, never a transform tree or a rendered row, so nothing else here
/// compares what the reader actually sees against what it should be.
///
/// Folds and blocks are read back off the incremental map to re-apply them,
/// which is the only way to name where they ended up. That makes this a
/// check that the rows rendered for a given fold and block set are right,
/// not that the positions were carried right. Carrying is covered by
/// `carried_fold_offsets_match_a_full_resolve` and
/// `blocks_follow_the_row_they_mark_across_an_edit`.
///
/// Blocks come from the map's carried list rather than from a rendered row,
/// because the tree holds each block as it was inserted. Reading a
/// placement off a snapshot names the row the block went in at, which
/// re-applies it to the wrong row once an edit has moved it.
#[test]
fn incremental_sync_matches_a_fresh_build_over_random_edits() {
    use super::{Block, CustomBlock};
    use stoat_text::{Anchor, Bias};

    // The same generator and seed derivation the fold-carrying test uses,
    // so a seed names the same thing in both.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state >> 33
    }

    // Tabs make the tab layer expand, the wide characters make it and the
    // wrap layer disagree about columns, and the combining mark rides on a
    // base character rather than standing alone. Plain rows in between keep
    // the ordinary path represented.
    let text: String = (0..30)
        .map(|i| match i % 4 {
            0 => format!("line{i}\twith a tab\n"),
            1 => format!("line{i} \u{5e7f}\u{4e1c}\u{8bdd} wide\n"),
            2 => format!("line{i} e\u{301}accented\n"),
            _ => format!("line{i} plain words here\n"),
        })
        .collect();

    for seed in 0..40u64 {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);

        let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
        let multi = MultiBuffer::singleton(shared.clone());
        let mut map = DisplayMap::new(multi, test_executor(), crate::test_notify());

        let wrap_width = match lcg(&mut state) % 3 {
            0 => None,
            _ => Some(5 + (lcg(&mut state) % 25) as u32),
        };
        map.set_wrap_width(wrap_width);

        // Disjoint and ordered, so the fold merge pass does not rewrite them
        // out from under the read-back below.
        let mut fold_ranges = Vec::new();
        let mut row = 0u32;
        while row < 26 && fold_ranges.len() < 3 {
            if lcg(&mut state).is_multiple_of(2) {
                let span = 1 + (lcg(&mut state) % 3) as u32;
                fold_ranges.push(Point::new(row, 0)..Point::new(row + span, 0));
                row += span;
            }
            row += 1 + (lcg(&mut state) % 3) as u32;
        }
        if !fold_ranges.is_empty() {
            map.fold(fold_ranges);
        }

        let blocks: Vec<BlockProperties> = (0..lcg(&mut state) % 4)
            .map(|i| {
                let row = (lcg(&mut state) % 28) as u32;
                let placement = match lcg(&mut state) % 3 {
                    0 => BlockPlacement::Above(row),
                    1 => BlockPlacement::Below(row),
                    _ => BlockPlacement::Replace {
                        start: row,
                        end: row + 1 + (lcg(&mut state) % 2) as u32,
                    },
                };
                BlockProperties::from_text(placement, vec![format!("block{i}")], BlockStyle::Fixed)
            })
            .collect();
        if !blocks.is_empty() {
            map.insert_blocks(blocks);
        }

        // Anchored at row starts, so an inlay never lands inside a wide
        // character. The anchors themselves are handed to both maps, since
        // they survive the edits below and resolve to the same final spots.
        let inlays: Vec<(Anchor, String, InlayKind)> = {
            let snap = map.multi_buffer.snapshot();
            (0..lcg(&mut state) % 3)
                .map(|i| {
                    let row = (lcg(&mut state) % 29) as u32;
                    let offset = snap.rope().point_to_offset(Point::new(row, 0));
                    (
                        snap.anchor_at(offset, Bias::Left),
                        format!(": hint{i}"),
                        InlayKind::Hint,
                    )
                })
                .collect()
        };
        if !inlays.is_empty() {
            map.splice_inlays(Vec::new(), inlays.clone());
        }

        for round in 0..4 {
            for _ in 0..1 + lcg(&mut state) % 3 {
                let len = shared.read().expect("poisoned").rope().len();
                let at = (lcg(&mut state) as usize) % (len + 1);

                // Both endpoints snap to grapheme boundaries. An edit
                // splitting one of the wide characters panics in the rope.
                let clip = |off: usize| {
                    shared
                        .read()
                        .expect("poisoned")
                        .rope()
                        .clip_to_grapheme_boundary(off.min(len), Bias::Left)
                };
                match lcg(&mut state) % 3 {
                    0 => {
                        let (start, end) = (clip(at), clip(at + 4));
                        shared.write().expect("poisoned").edit(start..end, "");
                    },
                    // A same-size replace moves no offset and changes no
                    // row count, which is the shape that defeats the
                    // version and touch mechanisms the layers carry state
                    // with.
                    1 => {
                        let (start, end) = (clip(at), clip(at + 2));
                        let width = end - start;
                        if width > 0 {
                            let filler = "q".repeat(width);
                            shared.write().expect("poisoned").edit(start..end, &filler);
                        }
                    },
                    _ => {
                        let start = clip(at);
                        shared.write().expect("poisoned").edit(start..start, "zz\n");
                    },
                }
            }

            let rows = {
                let snapshot = map.snapshot();
                (0..snapshot.line_count())
                    .map(|row| {
                        let kind = match snapshot.classify_row(row) {
                            BlockRowKind::BufferRow { buffer_row } => {
                                format!("buf{buffer_row}")
                            },
                            BlockRowKind::Block { block, line_index } => block.get_line(line_index),
                        };
                        (kind, snapshot.display_line(row))
                    })
                    .collect::<Vec<_>>()
            };

            // The folds and blocks as they now stand, which is the only
            // way to name where the edits left them.
            let carried_folds: Vec<Range<Point>> = {
                let snapshot = map.snapshot();
                let fold_snapshot = snapshot.fold_snapshot();
                let inlay_snapshot = snapshot.inlay_snapshot();
                fold_snapshot
                    .folds_in_range(InlayPoint::new(0, 0)..InlayPoint::new(u32::MAX, 0))
                    .into_iter()
                    .map(|fold| {
                        inlay_snapshot.to_buffer_point(fold.range.start)
                            ..inlay_snapshot.to_buffer_point(fold.range.end)
                    })
                    .collect()
            };
            // Read off the map's own carried list, not off a snapshot. The
            // transform tree holds each block as it was inserted, so a
            // placement taken from a rendered row names the row the block
            // went in at rather than the row it now occupies.
            //
            // Re-applied in id order, which is insertion order, rather than
            // the row order the list is kept in. Two blocks a fold collapses
            // onto one row are separated by priority and then by id, so a
            // fresh map only renders them the same way round if it mints
            // its ids in the same order.
            let carried_blocks: Vec<BlockProperties> = {
                let mut blocks: Vec<&Arc<CustomBlock>> =
                    map.block_map.custom_blocks.iter().collect();
                blocks.sort_by_key(|block| block.id);
                blocks
                    .into_iter()
                    .map(|block| {
                        let label = Block(Arc::clone(block)).get_line(0);
                        let mut props = BlockProperties::from_text(
                            block.placement,
                            vec![label],
                            BlockStyle::Fixed,
                        );
                        props.priority = block.priority;
                        props
                    })
                    .collect()
            };

            let fresh_rows = {
                let multi = MultiBuffer::singleton(shared.clone());
                let mut fresh = DisplayMap::new(multi, test_executor(), crate::test_notify());
                fresh.set_wrap_width(wrap_width);
                if !carried_folds.is_empty() {
                    fresh.fold(carried_folds);
                }
                if !carried_blocks.is_empty() {
                    fresh.insert_blocks(carried_blocks);
                }
                if !inlays.is_empty() {
                    fresh.splice_inlays(Vec::new(), inlays.clone());
                }

                let snapshot = fresh.snapshot();
                (0..snapshot.line_count())
                    .map(|row| {
                        let kind = match snapshot.classify_row(row) {
                            BlockRowKind::BufferRow { buffer_row } => {
                                format!("buf{buffer_row}")
                            },
                            BlockRowKind::Block { block, line_index } => block.get_line(line_index),
                        };
                        (kind, snapshot.display_line(row))
                    })
                    .collect::<Vec<_>>()
            };

            assert_eq!(
                rows.len(),
                fresh_rows.len(),
                "seed {seed} round {round}: row count",
            );
            for (row, (incremental, fresh)) in rows.iter().zip(&fresh_rows).enumerate() {
                assert_eq!(incremental, fresh, "seed {seed} round {round} row {row}",);
            }
        }
    }
}

/// The stack end to end, over generated buffers, hints, folds, wrap widths
/// and block sets.
///
/// Each layer's own suite holds it against the layer below. Nothing holds
/// the composition against the buffer, which is where an error every layer
/// reports consistently to its neighbour compounds into a display position
/// naming the wrong text.
///
/// The round trip is stated from the buffer side, because going the other
/// way and requiring the display point back is too strong under soft wrap.
/// The end of a wrapped row and the start of its continuation are one
/// buffer position, both are positions a caret can occupy, and
/// [`DisplaySnapshot::buffer_to_display`] has to pick one of them.
///
/// The round trip is skipped for a document whose every row a `Replace`
/// block swallowed. There is no buffer position to answer with there, so
/// requiring one would assert something false.
#[test]
fn the_display_stack_round_trips_and_measures_what_it_paints() {
    use stoat_text::Bias;
    for seed in 0..256 {
        let mut display_map = sampler::random_display_map(seed);
        let snapshot = display_map.snapshot();
        let addressable = (0..snapshot.line_count())
            .any(|row| matches!(snapshot.classify_row(row), BlockRowKind::BufferRow { .. }));

        for row in 0..snapshot.line_count() {
            for column in 0..=snapshot.line_len(row) + 1 {
                for bias in [Bias::Left, Bias::Right] {
                    let clipped = snapshot.clip_point(DisplayPoint::new(row, column), bias);
                    assert_eq!(
                        snapshot.clip_point(clipped, bias),
                        clipped,
                        "seed {seed}: {row}:{column} clipped {bias:?} moves again",
                    );

                    if !addressable {
                        continue;
                    }
                    let buffer = snapshot.display_to_buffer(clipped).unwrap_or_else(|| {
                        panic!("seed {seed}: {row}:{column} {bias:?} clips off the buffer")
                    });
                    // A `Replace` block can hide the row a buffer position
                    // is shown on, and then it has no display position to
                    // read back from. That is the block layer's contract,
                    // not a mismatch.
                    let shown = snapshot.buffer_to_display(buffer);
                    let Some(back) = snapshot.display_to_buffer(shown) else {
                        continue;
                    };
                    assert_eq!(
                        back, buffer,
                        "seed {seed}: {row}:{column} {bias:?} names {buffer:?}, which does \
                             not survive being shown and read back",
                    );
                }
            }
        }

        // Ascending buffer points give non-descending display points. Not
        // strictly ascending, since a fold collapses a whole range onto the
        // one position that stands for it.
        let rope = snapshot.buffer_snapshot().rope();
        let mut previous: Option<(Point, DisplayPoint)> = None;
        for row in 0..=rope.max_point().row {
            for column in 0..=rope.line_len(row) {
                let point = Point::new(row, column);
                if rope.clip_point(point, Bias::Left) != point {
                    continue;
                }

                let display = snapshot.buffer_to_display(point);
                if let Some((last_point, last_display)) = previous {
                    assert!(
                        display >= last_display,
                        "seed {seed}: {last_point:?} shows at {last_display:?} but the later \
                             {point:?} shows at {display:?}",
                    );
                }
                previous = Some((point, display));
            }
        }

        for row in 0..snapshot.line_count() {
            let painted: u32 = snapshot
                .highlighted_chunks(row..row + 1)
                .map(|chunk| {
                    chunk
                        .text
                        .chars()
                        .filter(|&ch| ch != '\n')
                        .map(display_width)
                        .sum::<u32>()
                })
                .sum();
            assert_eq!(
                snapshot.line_len(row),
                painted,
                "seed {seed}: row {row} measures a width it does not paint",
            );
        }
    }
}

/// Wrapping only ever splits a row into more rows, so a wrap snapshot can
/// never show fewer than the fold layer it holds.
///
/// Two halves of one snapshot disagreeing is invisible where it happens and
/// surfaces far away, as a coordinate resolving past the end of a document
/// that is not actually that short.
#[test]
fn a_wrap_snapshot_shows_at_least_the_rows_beneath_it() {
    for seed in 0..256 {
        let mut display_map = sampler::random_display_map(seed);
        let wrap = display_map.sync_through_wrap().snapshot;
        let fold_rows = wrap.tab_snapshot().fold_snapshot().line_count();

        assert!(
            wrap.line_count() >= fold_rows,
            "seed {seed}: wrap shows {} rows over a fold layer of {fold_rows}, at width {:?}",
            wrap.line_count(),
            wrap.wrap_width(),
        );
    }
}
