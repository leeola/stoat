//! Navigation between the two ends of a moved block of code.
//!
//! A structural diff can tell that lines did not appear or vanish but travelled
//! -- out of one file into another, or from one place in a file to another.
//! That relation has two ends: the source the lines came from and the target
//! they went to. These handlers resolve whichever end the cursor is not on and
//! jump there, so a reviewer reading a moved hunk can see where it came from
//! without searching for it.
//!
//! The cursor can sit at either end, or on neither. A moved-away seam leaves an
//! empty hunk at the old site, and an intra-file move is consumed into its
//! moved-to hunk so no hunk marks the source at all, which is why
//! [`current_move_summary`] hunts three ways before giving up.
//!
//! A move may have several candidate sources, since identical lines can come
//! from more than one place. The editor holds a cursor into that list, keyed by
//! the hunk line so it resets when the reader moves to another hunk.

use super::{focused_editor_mut, movement};
use crate::{
    app::{Stoat, UpdateEffect},
    diff_map::{DiffHunkStatus, TokenDetail},
};
use stoat_language::structural_diff::BufferRef;

#[derive(Copy, Clone, Debug)]
pub(super) enum MoveNavigation {
    FirstSource,
    NextSource,
    PrevSource,
    Target,
}

/// Per-source navigation target carried by [`MoveSummary`]: the line
/// to land on plus the optional foreign-buffer pointer. `buffer ==
/// None` means the source lives in the same file (and same focused
/// editor) as the hunk under the cursor; `buffer == Some(_)` means
/// the source lives in a different file and [`navigate`] must open or
/// focus that buffer before positioning the cursor.
#[derive(Clone, Debug)]
pub(super) struct MoveSourceRef {
    pub(super) line: u32,
    pub(super) buffer: Option<BufferRef>,
}

/// Resolved move-provenance summary for the hunk under the editor's
/// cursor. Used by the move-navigation action handlers.
pub(super) struct MoveSummary {
    /// Line the hunk starts on in the buffer.
    pub(super) hunk_line: u32,
    /// Candidate source locations, zero or more.
    pub(super) source_refs: Vec<MoveSourceRef>,
    /// If the hunk is the LHS side of a move, the paired RHS target.
    pub(super) target_ref: Option<MoveSourceRef>,
    /// Number of candidate sources (>1 = ambiguous move).
    pub(super) source_count: usize,
}

pub(super) fn current_move_summary(stoat: &mut Stoat) -> Option<MoveSummary> {
    let editor = focused_editor_mut(stoat)?;
    let snapshot = editor.display_map.snapshot();
    let anchor = editor.selections.newest_anchor().start;
    let buffer_snapshot = snapshot.buffer_snapshot();
    let offset = buffer_snapshot.resolve_anchor(&anchor);
    let cursor_line = buffer_snapshot.rope().offset_to_point(offset).row;

    // A cursor on a moved line (the moved-to side) summarizes its own detail.
    if snapshot.line_diff_status(cursor_line) == crate::host::DiffStatus::Moved {
        return move_summary_from_detail(snapshot.token_detail_for_line(cursor_line)?, cursor_line);
    }

    let diff_map = snapshot.diff_map()?;

    // The cursor may sit on a moved-away seam, an LHS-only Moved hunk with an
    // empty line range anchored here. Its base-side metadata records the moved-to
    // counterpart, so its detail yields the forward target.
    if let Some(hunk) = diff_map.hunks().find(|h| {
        h.status == DiffHunkStatus::Moved
            && h.buffer_line_range.is_empty()
            && h.buffer_start_line == cursor_line
    }) && let Some(detail) = &hunk.token_detail
    {
        return move_summary_from_detail(detail, cursor_line);
    }

    // Cursor on an intra-file move's source, consumed into the moved-to hunk so
    // no hunk sits at the old site. Find the Moved hunk whose intra-file metadata
    // source lands here and jump forward to its moved-to line.
    for hunk in diff_map.hunks() {
        if hunk.status != DiffHunkStatus::Moved {
            continue;
        }
        let Some(detail) = &hunk.token_detail else {
            continue;
        };
        let lands_here = detail
            .buffer_spans
            .iter()
            .chain(detail.base_spans.iter())
            .filter_map(|s| s.move_metadata.as_ref())
            .flat_map(|m| m.sources.iter())
            .any(|s| s.buffer.is_none() && s.line_range.contains(&cursor_line));
        if lands_here {
            return Some(MoveSummary {
                hunk_line: cursor_line,
                source_count: 0,
                source_refs: Vec::new(),
                target_ref: Some(MoveSourceRef {
                    line: hunk.buffer_start_line,
                    buffer: None,
                }),
            });
        }
    }

    None
}

/// Build a [`MoveSummary`] from a moved hunk's [`TokenDetail`].
///
/// `source_refs` are the move's candidate counterpart locations. `target_ref` is
/// set only for the base (moved-away) side of a move, where `buffer_spans` is
/// empty and `base_spans` is not, and the metadata records the moved-to line.
fn move_summary_from_detail(detail: &TokenDetail, hunk_line: u32) -> Option<MoveSummary> {
    let metadata = detail
        .buffer_spans
        .iter()
        .chain(detail.base_spans.iter())
        .find_map(|s| s.move_metadata.clone())?;
    let source_refs: Vec<MoveSourceRef> = metadata
        .sources
        .iter()
        .map(|s| MoveSourceRef {
            line: s.line_range.start,
            buffer: s.buffer.clone(),
        })
        .collect();
    let target_ref = if detail.buffer_spans.is_empty() && !detail.base_spans.is_empty() {
        metadata.sources.first().map(|s| MoveSourceRef {
            line: s.line_range.start,
            buffer: s.buffer.clone(),
        })
    } else {
        None
    };
    Some(MoveSummary {
        hunk_line,
        source_count: metadata.sources.len(),
        source_refs,
        target_ref,
    })
}

pub(super) fn navigate(stoat: &mut Stoat, nav: MoveNavigation) -> UpdateEffect {
    let Some(summary) = current_move_summary(stoat) else {
        return UpdateEffect::None;
    };
    if summary.source_refs.is_empty() && summary.target_ref.is_none() {
        return UpdateEffect::None;
    }

    let target_ref: Option<MoveSourceRef> = {
        let Some(editor) = focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        match nav {
            MoveNavigation::FirstSource => {
                editor.move_source_cursor = Some((summary.hunk_line, 0));
                summary.source_refs.first().cloned()
            },
            MoveNavigation::NextSource => {
                let idx = match editor.move_source_cursor {
                    Some((line, i)) if line == summary.hunk_line => {
                        (i + 1) % summary.source_refs.len().max(1)
                    },
                    _ => 0,
                };
                editor.move_source_cursor = Some((summary.hunk_line, idx));
                summary.source_refs.get(idx).cloned()
            },
            MoveNavigation::PrevSource => {
                let len = summary.source_refs.len().max(1);
                let idx = match editor.move_source_cursor {
                    Some((line, i)) if line == summary.hunk_line => (i + len - 1) % len,
                    _ => len.saturating_sub(1),
                };
                editor.move_source_cursor = Some((summary.hunk_line, idx));
                summary.source_refs.get(idx).cloned()
            },
            MoveNavigation::Target => summary.target_ref,
        }
    };

    let Some(target_ref) = target_ref else {
        return UpdateEffect::None;
    };

    if let Some(buffer_ref) = target_ref.buffer.as_ref() {
        let focused = stoat.active_workspace().panes.focus();
        if crate::buffer_lifecycle::open_file_in_pane(stoat, focused, &buffer_ref.path).is_none() {
            return UpdateEffect::None;
        }
    }

    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    movement::set_cursor_row(editor, target_ref.line);
    UpdateEffect::Redraw
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        diff_map::{ChangeKind as DmChangeKind, ChangeSpan, DiffHunk, DiffMap},
        pane::View,
        test_harness::{
            editor::{focused_buffer_path, focused_head_row},
            TestHarness,
        },
    };
    use std::{ops::Range, path::Path, sync::Arc};
    use stoat_language::structural_diff::{MoveMetadata, MoveSource, Side};

    fn install_moved_hunk_to_other_file(
        h: &mut TestHarness,
        moved_line: u32,
        target_path: &Path,
        target_line: u32,
    ) {
        let buffer_ref = BufferRef {
            path: target_path.to_path_buf(),
            fingerprint: [7u8; 32],
        };
        let metadata = Arc::new(MoveMetadata {
            sources: vec![MoveSource {
                buffer: Some(buffer_ref),
                side: Side::Lhs,
                byte_range: 0..0,
                line_range: target_line..(target_line + 1),
            }],
        });
        let detail = Arc::new(TokenDetail {
            buffer_spans: vec![ChangeSpan {
                byte_range: 0..0,
                kind: DmChangeKind::Moved,
                move_metadata: Some(metadata),
            }],
            base_spans: Vec::new(),
        });
        let hunk = DiffHunk {
            status: DiffHunkStatus::Moved,
            unstaged_lines: std::iter::once(moved_line..(moved_line + 1)).collect(),
            buffer_start_line: moved_line,
            buffer_line_range: moved_line..(moved_line + 1),
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: Some(detail),
        };
        let dm = DiffMap::from_hunks([hunk], None);
        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        guard.diff_map = Some(dm);
    }

    /// Install `dm` onto the focused editor's buffer.
    fn attach_diff_map(h: &mut TestHarness, dm: DiffMap) {
        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        buffer.write().expect("poisoned").diff_map = Some(dm);
    }

    /// A one-source Moved hunk over line `moved_line`, plus its `TokenDetail`.
    /// `buffer_spans_present` selects the moved-to (RHS) side vs. the moved-away
    /// (LHS) base side, and an empty `line_range` makes it a seam.
    fn moved_hunk(
        line_range: Range<u32>,
        buffer_spans_present: bool,
        source: MoveSource,
    ) -> DiffHunk {
        let span = ChangeSpan {
            byte_range: 0..0,
            kind: DmChangeKind::Moved,
            move_metadata: Some(Arc::new(MoveMetadata {
                sources: vec![source],
            })),
        };
        let (buffer_spans, base_spans) = if buffer_spans_present {
            (vec![span], Vec::new())
        } else {
            (Vec::new(), vec![span])
        };
        DiffHunk {
            status: DiffHunkStatus::Moved,
            unstaged_lines: vec![line_range.clone()],
            buffer_start_line: line_range.start,
            buffer_line_range: line_range,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: Some(Arc::new(TokenDetail {
                buffer_spans,
                base_spans,
            })),
        }
    }

    fn intra_source(line: u32) -> MoveSource {
        MoveSource {
            buffer: None,
            side: Side::Lhs,
            byte_range: 0..0,
            line_range: line..(line + 1),
        }
    }

    #[test]
    fn move_nav_jumps_to_foreign_buffer_path() {
        let mut h = TestHarness::with_size(40, 10);
        let a_path = h.write_file("a.rs", "a0\na1\na2\na3\na4\n");
        let b_path = h.write_file("b.rs", "b0\nb1\nb2\nb3\nb4\n");
        h.open_file(&a_path);
        install_moved_hunk_to_other_file(&mut h, 2, &b_path, 3);

        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            movement::set_cursor_row(editor, 2);
        }
        assert_eq!(
            focused_head_row(&mut h.stoat),
            2,
            "cursor on the moved hunk in a.rs"
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpToMoveSource);

        assert_eq!(
            focused_buffer_path(&h.stoat),
            b_path,
            "focused pane switched to b.rs"
        );
        assert_eq!(
            focused_head_row(&mut h.stoat),
            3,
            "cursor on the source line in b.rs"
        );
    }

    #[test]
    fn move_nav_intra_file_stays_in_buffer() {
        let mut h = TestHarness::with_size(40, 10);
        let a_path = h.write_file("a.rs", "a0\na1\na2\na3\na4\n");
        h.open_file(&a_path);

        let metadata = Arc::new(MoveMetadata {
            sources: vec![MoveSource {
                buffer: None,
                side: Side::Lhs,
                byte_range: 0..0,
                line_range: 4..5,
            }],
        });
        let detail = Arc::new(TokenDetail {
            buffer_spans: vec![ChangeSpan {
                byte_range: 0..0,
                kind: DmChangeKind::Moved,
                move_metadata: Some(metadata),
            }],
            base_spans: Vec::new(),
        });
        let hunk = DiffHunk {
            status: DiffHunkStatus::Moved,
            unstaged_lines: std::iter::once(2..3).collect(),
            buffer_start_line: 2,
            buffer_line_range: 2..3,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: Some(detail),
        };
        {
            let dm = DiffMap::from_hunks([hunk], None);
            let ws = h.stoat.active_workspace();
            let focused = ws.panes.focus();
            let editor_id = match ws.panes.pane(focused).view {
                View::Editor(id) => id,
                _ => panic!("focused pane is not an editor"),
            };
            let buffer_id = ws.editors[editor_id].buffer_id;
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            let mut guard = buffer.write().expect("poisoned");
            guard.diff_map = Some(dm);
        }

        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            movement::set_cursor_row(editor, 2);
        }
        assert_eq!(focused_head_row(&mut h.stoat), 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpToMoveSource);

        assert_eq!(
            focused_buffer_path(&h.stoat),
            a_path,
            "stayed in a.rs (intra-file move)"
        );
        assert_eq!(
            focused_head_row(&mut h.stoat),
            4,
            "cursor on the source line in a.rs"
        );
    }

    #[test]
    fn move_nav_target_from_moved_away_seam() {
        let mut h = TestHarness::with_size(40, 10);
        let path = h.write_file("a.rs", "a0\na1\na2\na3\na4\n");
        h.open_file(&path);

        // An LHS-only seam at line 1 whose base-side metadata points at the
        // moved-to counterpart on line 4.
        let hunk = moved_hunk(
            1..1,
            false,
            MoveSource {
                buffer: None,
                side: Side::Rhs,
                byte_range: 0..0,
                line_range: 4..5,
            },
        );
        attach_diff_map(&mut h, DiffMap::from_hunks([hunk], None));

        {
            let editor = focused_editor_mut(&mut h.stoat).expect("editor");
            movement::set_cursor_row(editor, 1);
        }
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpToMoveTarget);
        assert_eq!(
            focused_head_row(&mut h.stoat),
            4,
            "M from the moved-away seam jumps to the moved-to line"
        );
    }

    #[test]
    fn move_nav_target_from_intra_file_source() {
        let mut h = TestHarness::with_size(40, 10);
        let path = h.write_file("a.rs", "a0\na1\na2\na3\na4\n");
        h.open_file(&path);

        // The moved-to hunk sits on line 3. Its metadata records the source on
        // line 1, where no hunk sits (consumed into the moved-to hunk).
        let hunk = moved_hunk(3..4, true, intra_source(1));
        attach_diff_map(&mut h, DiffMap::from_hunks([hunk], None));

        {
            let editor = focused_editor_mut(&mut h.stoat).expect("editor");
            movement::set_cursor_row(editor, 1);
        }
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpToMoveTarget);
        assert_eq!(
            focused_head_row(&mut h.stoat),
            3,
            "M from an intra-file source jumps forward to the moved-to line"
        );
    }

    #[test]
    fn move_nav_cycles_two_sources() {
        let mut h = TestHarness::with_size(40, 10);
        let path = h.write_file("a.rs", "a0\na1\na2\na3\na4\n");
        h.open_file(&path);

        // A moved-to hunk on line 2 with two intra-file source candidates.
        let hunk = DiffHunk {
            token_detail: Some(Arc::new(TokenDetail {
                buffer_spans: vec![ChangeSpan {
                    byte_range: 0..0,
                    kind: DmChangeKind::Moved,
                    move_metadata: Some(Arc::new(MoveMetadata {
                        sources: vec![intra_source(0), intra_source(4)],
                    })),
                }],
                base_spans: Vec::new(),
            })),
            ..moved_hunk(2..3, true, intra_source(0))
        };
        attach_diff_map(&mut h, DiffMap::from_hunks([hunk], None));

        // Each jump lands on a source and moves the cursor, so return to the
        // moved-to line before the next to keep cycling from the same hunk.
        let mut jump = |action: &dyn stoat_action::Action| -> u32 {
            {
                let editor = focused_editor_mut(&mut h.stoat).expect("editor");
                movement::set_cursor_row(editor, 2);
            }
            crate::action_handlers::dispatch(&mut h.stoat, action);
            focused_head_row(&mut h.stoat)
        };
        assert_eq!(
            jump(&stoat_action::JumpToNextMoveSource),
            0,
            "first next -> source 0"
        );
        assert_eq!(
            jump(&stoat_action::JumpToNextMoveSource),
            4,
            "second next -> source 1"
        );
        assert_eq!(
            jump(&stoat_action::JumpToNextMoveSource),
            0,
            "third next wraps"
        );
        assert_eq!(
            jump(&stoat_action::JumpToPrevMoveSource),
            4,
            "prev steps back"
        );
    }
}
