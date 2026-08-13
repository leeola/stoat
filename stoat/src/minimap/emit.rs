//! The app half of the minimap: what to declare, what to fill, and when.
//!
//! The engine beside this module summarizes lines; nothing there knows the app.
//! This is where a summary becomes a frame. It decides which strips the current
//! layout declares, keeps each one's content store keyed to its buffer, and
//! drains the store's splices into `minimap_lines` frames as the buffer's parse,
//! diagnostics, and diff move underneath it.
//!
//! A strip's content is versioned rather than resent. A parse or an LSP token
//! refresh bumps the version the store synced at, and only the lines that
//! actually changed cross the wire, so a keystroke costs a few rows instead of
//! the file.

use crate::{
    app::Stoat,
    buffer::BufferId,
    display_map::{DisplayPoint, DisplaySnapshot},
    editor_state::EditorId,
    minimap::EdgeSource,
    pane::View,
    workspace::WorkspaceId,
};
use std::{
    cell::OnceCell,
    collections::hash_map::{DefaultHasher, Entry},
    hash::{Hash, Hasher},
    ops::Range,
};
use stoat_config::MinimapMode;
use stoat_text::Bias;

/// Assign a `content_id` to each visible split-editor buffer a minimap strip
/// may render, so the declare widget can read the id while the frame paints.
///
/// The summaries themselves sync afterward at the frame seam in
/// [`emit_minimap`]. A no-op with the minimap off.
pub(crate) fn ensure_minimap_content_ids(stoat: &mut Stoat) {
    if !stoat.minimap_enabled() {
        return;
    }
    let ws_id = stoat.active_workspace;
    // The walk reads the workspace while the declaration writes to the
    // content store, so the two fields are borrowed apart rather than the
    // buffer ids being collected to get one loop out of the way of the
    // other.
    let Stoat {
        workspaces,
        minimap_content,
        minimap_next_content_id,
        ..
    } = stoat;
    let ws = &workspaces[ws_id];

    for (_, pane) in ws.panes.split_panes() {
        let View::Editor(editor_id) = pane.view else {
            continue;
        };
        let Some(editor) = ws.editors.get(editor_id) else {
            continue;
        };
        if let Entry::Vacant(slot) = minimap_content.entry((ws_id, editor.buffer_id)) {
            slot.insert(crate::minimap::MinimapContent::new(
                *minimap_next_content_id,
            ));
            *minimap_next_content_id += 1;
        }
    }
}

/// Sync each visible minimap strip's buffer and drain its summary changes to
/// the terminal as `minimap_lines`, retiring content for buffers that closed.
///
/// Runs at the frame seam after [`Stoat::emit_smooth_scroll`], so each editor's
/// reserved strip rect from the paint is current. The strip declaration rides
/// the diffed scene from the paint. This sends only the persistent content
/// stores. A no-op until [`Stoat::stoatty`] confirms a listener and the APC
/// channel is installed, or with the minimap off.
pub(crate) fn emit_minimap(stoat: &mut Stoat) {
    // Recomputed below from each synced strip. Cleared first so the early
    // exits (no listener, no channel, minimap off) leave no build pending.
    stoat.minimap_build_pending = false;
    let Some(apc_tx) = stoat.apc_tx.clone().filter(|_| stoat.stoatty) else {
        return;
    };
    let ws_id = stoat.active_workspace;

    // Per-pane mode syncs a strip only where one is reserved (minimap_rect).
    // Single mode has no per-pane rects, so it syncs every visible plain
    // editor's buffer, keeping all of them warm for instant focus switches.
    let single = stoat.minimap_mode() == MinimapMode::Single;
    let strips: Vec<(BufferId, EditorId)> = {
        let ws = &stoat.workspaces[ws_id];
        ws.panes
            .split_panes()
            .filter_map(|(_, pane)| {
                let View::Editor(editor_id) = pane.view else {
                    return None;
                };
                let editor = ws.editors.get(editor_id)?;
                let included = if single {
                    editor.review_view.is_none() && !editor.diff_view
                } else {
                    editor.minimap_rect.is_some()
                };
                included.then_some((editor.buffer_id, editor_id))
            })
            .collect()
    };

    let mut out = Vec::new();
    for (buffer_id, editor_id) in strips {
        sync_minimap_strip(stoat, ws_id, buffer_id, editor_id, &mut out);
        stoat.minimap_build_pending |= stoat
            .minimap_content
            .get(&(ws_id, buffer_id))
            .is_some_and(|content| content.build_pending());
    }

    let dropped: Vec<(WorkspaceId, BufferId)> = stoat
        .minimap_content
        .keys()
        .filter(|(ws, buffer_id)| {
            *ws == ws_id && stoat.workspaces[ws_id].buffers.get(*buffer_id).is_none()
        })
        .copied()
        .collect();
    for key in dropped {
        if let Some(content) = stoat.minimap_content.remove(&key) {
            stoatty_protocol::command::encode_minimap_drop_into(
                &mut out,
                &stoatty_protocol::command::MinimapDropCommand {
                    content_id: content.content_id(),
                },
            );
        }
    }

    if !out.is_empty() {
        let _ = apc_tx.send(out);
    }
}

/// Sync one strip's [`crate::minimap::MinimapContent`] to its buffer and
/// append the drained splices to `out` as a `minimap_lines` frame.
fn sync_minimap_strip(
    stoat: &mut Stoat,
    ws_id: WorkspaceId,
    buffer_id: BufferId,
    editor_id: EditorId,
    out: &mut Vec<u8>,
) {
    let (content_id, synced_version) = match stoat.minimap_content.get(&(ws_id, buffer_id)) {
        Some(content) => (content.content_id(), content.synced_version()),
        None => return,
    };

    let buffer_syntax_version = stoat.workspaces[ws_id].buffers.syntax_version(buffer_id);
    let lsp_token_version = stoat.workspaces[ws_id].buffers.lsp_token_version(buffer_id);
    let (snapshot, diff_version, diag_version, severity_map) =
        match stoat.workspaces[ws_id].editors.get_mut(editor_id) {
            Some(editor) => {
                let snapshot = editor.display_map.snapshot();
                let diff_version = editor.display_map.diff_version();
                let (diag_version, severity_map) = editor
                    .gutter_severity_cache
                    .as_ref()
                    .map(|cache| (cache.version, cache.map.clone()))
                    .unwrap_or_default();
                (snapshot, diff_version, diag_version, severity_map)
            },
            None => return,
        };
    let (rope, version, edits) = {
        let buf_snap = snapshot.buffer_snapshot();
        (
            buf_snap.rope().clone(),
            buf_snap.version(),
            buf_snap.edits_since(synced_version),
        )
    };

    let decoration_version = {
        let mut hasher = DefaultHasher::new();
        diff_version.hash(&mut hasher);
        diag_version.hash(&mut hasher);
        hasher.finish()
    };
    let syntax_version = minimap_syntax_version(
        stoat.syntax_highlight,
        buffer_syntax_version,
        lsp_token_version,
    );
    let syntax_other_version =
        minimap_syntax_other_version(stoat.syntax_highlight, lsp_token_version);

    let syntax_on = stoat.syntax_highlight;
    let class_table = &stoat.minimap_class_table;

    // Resolve only the tokens overlapping the rows each sync branch touches, so
    // an edit or recolor never resolves the whole buffer. A steady frame that
    // summarizes nothing queries no range and pays nothing.
    let tokens_for =
        |rows: Range<u32>| minimap_line_tokens(&snapshot, buffer_id, syntax_on, class_table, rows);
    let marks = MinimapEdges {
        snapshot: &snapshot,
        diff: OnceCell::new(),
        severity_map: &severity_map,
        class_table,
    };

    let content = stoat
        .minimap_content
        .get_mut(&(ws_id, buffer_id))
        .expect("checked above");
    content.sync(
        &rope,
        version,
        &edits,
        crate::minimap::SyncVersions {
            decoration: decoration_version,
            syntax: syntax_version,
            syntax_other: syntax_other_version,
        },
        tokens_for,
        marks,
    );

    for splice in content.take_queued() {
        stoatty_protocol::command::encode_minimap_lines_into(
            out,
            &stoatty_protocol::command::MinimapLinesCommand {
                content_id,
                start: splice.start,
                removed: splice.removed,
                lines: splice.lines.into_iter().map(convert_minimap_runs).collect(),
            },
        );
    }
}

/// Content version of a minimap strip's coloring, hashing the inputs whose
/// change must re-summarize every row already built.
///
/// The parse and LSP versions stay [`Option`]s rather than collapsing to a
/// sentinel. Both count *buffer* versions and a freshly opened buffer sits at
/// version 0, so flattening `None` to 0 would make "not parsed yet" and "parsed
/// the untouched file" hash alike. Any buffer past the sync-parse cap summarizes
/// its rows before its first parse lands, and that collision would leave them
/// monochrome until an unrelated edit re-summarized them.
fn minimap_syntax_version(syntax_on: bool, parse: Option<u64>, lsp: Option<u64>) -> u64 {
    let mut hasher = DefaultHasher::new();
    syntax_on.hash(&mut hasher);
    parse.hash(&mut hasher);
    lsp.hash(&mut hasher);
    hasher.finish()
}

/// The part of [`minimap_syntax_version`] the parse does not contribute.
///
/// Only the parse reports which rows its tokens changed, so the strip needs to
/// tell a bump it has row information for from one it does not. A change here
/// puts the recolor sweep back to covering every built row.
fn minimap_syntax_other_version(syntax_on: bool, lsp: Option<u64>) -> u64 {
    let mut hasher = DefaultHasher::new();
    syntax_on.hash(&mut hasher);
    lsp.hash(&mut hasher);
    hasher.finish()
}

/// The viewport's top and span in buffer lines, for a minimap view emit.
///
/// A minimap strip is one row per buffer line, but the editor scrolls in display
/// rows, which soft-wrap, folds, and block rows all inflate. Emitting the display
/// figures unconverted overshoots the strip's scrollable span, so the terminal
/// clamps its ratio while the thumb offset keeps climbing and the thumb slides
/// off the strip's bottom edge.
///
/// The returned top is fractional so a sub-row glide still moves the thumb. It
/// interpolates toward the next display row's line rather than adding the raw
/// fraction, because several display rows of one wrapped line share a buffer
/// line and a raw fraction would run the thumb backward on each row boundary.
///
/// The span is at least 1, so an empty or single-line viewport still leaves the
/// terminal a non-degenerate window to place the thumb in.
pub(crate) fn minimap_view_window(
    snapshot: &DisplaySnapshot,
    scroll_offset: f32,
    viewport_rows: u32,
) -> (f32, u16) {
    let display_top = scroll_offset.max(0.0).floor();
    let top_row = display_top as u32;
    let top_line = minimap_buffer_line(snapshot, top_row);

    let next_line = minimap_buffer_line(snapshot, top_row.saturating_add(1));
    let step = next_line.saturating_sub(top_line) as f32;
    let top = top_line as f32 + (scroll_offset - display_top) * step;

    let last_row = top_row
        .saturating_add(viewport_rows.max(1))
        .saturating_sub(1);
    let end_line = minimap_buffer_line(snapshot, last_row)
        .saturating_add(1)
        .min(snapshot.buffer_line_count());
    let visible = end_line.saturating_sub(top_line).max(1);

    (top, visible.min(u16::MAX as u32) as u16)
}

/// The buffer line a display row sits on.
///
/// A block row belongs to no buffer line, so it resolves to the line of the
/// nearest row above it -- the line the block annotates.
fn minimap_buffer_line(snapshot: &DisplaySnapshot, display_row: u32) -> u32 {
    let clipped = snapshot.clip_ignoring_line_ends(DisplayPoint::new(display_row, 0), Bias::Left);
    snapshot
        .display_to_buffer(clipped)
        .map_or(0, |point| point.row)
}

/// Resolve a buffer's syntax highlights into minimap line tokens bucketed by row.
///
/// Reads the tree-sitter and LSP semantic tokens, which are buffer-anchored, so
/// the byte ranges are exact regardless of tab expansion, soft-wrap, or inlays --
/// unlike display chunks. Each token splits across the buffer lines it spans, and
/// the pieces bucket per row as line-relative [`crate::minimap::LineToken`]s
/// carrying their scope's palette class. Tokens whose style names no syntax
/// scope classify 0 and drop, and `syntax_on` off yields an empty map.
pub(crate) fn minimap_line_tokens(
    snapshot: &DisplaySnapshot,
    buffer_id: BufferId,
    syntax_on: bool,
    class_table: &crate::minimap::ClassTable,
    rows: Range<u32>,
) -> std::collections::HashMap<u32, Vec<crate::minimap::LineToken>> {
    let mut by_row: std::collections::HashMap<u32, Vec<crate::minimap::LineToken>> =
        std::collections::HashMap::new();
    if !syntax_on || rows.is_empty() {
        return by_row;
    }

    let buffer_snap = snapshot.buffer_snapshot();
    let rope = buffer_snap.rope();

    let last_row = rows.end.min(rope.max_point().row + 1).saturating_sub(1);
    if rows.start > last_row {
        return by_row;
    }
    // The byte span of `rows`, so each channel resolves anchors only for the
    // tokens that can overlap it instead of the whole buffer.
    let byte_range = {
        let start = rope.point_to_offset(stoat_text::Point::new(rows.start, 0));
        let end = rope.point_to_offset(stoat_text::Point::new(last_row, rope.line_len(last_row)));
        start..end
    };

    for highlights in [
        snapshot.semantic_token_highlights(),
        snapshot.lsp_token_highlights(),
    ] {
        let Some(channel) = highlights.get(&buffer_id) else {
            continue;
        };
        let bounds =
            channel.overlap_bounds(&byte_range, |anchor| buffer_snap.resolve_anchor(anchor));
        for span in channel.range(bounds) {
            let class = class_table.class_of(span.style);
            if class == 0 {
                continue;
            }
            let start = buffer_snap.resolve_anchor(&span.range.start);
            let end = buffer_snap.resolve_anchor(&span.range.end);
            if start >= end {
                continue;
            }

            let start_row = rope.offset_to_point(start).row.max(rows.start);
            let end_row = rope.offset_to_point(end).row.min(last_row);
            for row in start_row..=end_row {
                let line_start = rope.point_to_offset(stoat_text::Point::new(row, 0));
                let line_end =
                    rope.point_to_offset(stoat_text::Point::new(row, rope.line_len(row)));
                let s = start.max(line_start);
                let e = end.min(line_end);
                if s < e {
                    by_row
                        .entry(row)
                        .or_default()
                        .push(crate::minimap::LineToken {
                            range: (s - line_start)..(e - line_start),
                            class,
                        });
                }
            }
        }
    }

    for tokens in by_row.values_mut() {
        tokens.sort_by_key(|token| token.range.start);
    }
    by_row
}

/// One editor's minimap edge-lane marks, sourced from its diagnostics and its
/// diff.
///
/// Resolving a single row means a seek into the hunk tree, so the bulk answer
/// matters as much as the per-row one. Both structures behind a mark are keyed
/// by row, so which rows are markable at all comes out of one ordered walk
/// rather than a seek per line of the file.
struct MinimapEdges<'a> {
    snapshot: &'a DisplaySnapshot,
    /// Where the hunks sit now, resolved on the first row asked for rather than
    /// at construction.
    ///
    /// Every branch of a strip sync is gated, and a strip whose buffer, marks
    /// and colors all held still since the last frame takes none of them, so
    /// most frames ask nothing. The resolve is an ordered walk of the buffer's
    /// fragments, which is far too much to spend on a question that is usually
    /// not put. Held once for the pass, since `edge_of` runs per row and the
    /// diff job's own rows are already behind.
    diff: OnceCell<Option<crate::diff_map::LiveHunks<'a>>>,
    severity_map: &'a std::collections::BTreeMap<u32, lsp_types::DiagnosticSeverity>,
    class_table: &'a crate::minimap::ClassTable,
}

impl<'a> MinimapEdges<'a> {
    fn live_hunks(&self) -> Option<&crate::diff_map::LiveHunks<'a>> {
        self.diff
            .get_or_init(|| {
                self.snapshot
                    .diff_map()
                    .map(|diff| diff.live_hunks(self.snapshot.buffer_snapshot()))
            })
            .as_ref()
    }
}

impl EdgeSource for MinimapEdges<'_> {
    fn edge_of(&self, row: u32) -> Option<u8> {
        minimap_edge_class(self.live_hunks(), self.severity_map, self.class_table, row)
    }

    fn marked_rows(&self, rows: Range<u32>) -> Vec<u32> {
        let mut marked: Vec<u32> = self
            .severity_map
            .range(rows.clone())
            .map(|(&row, _)| row)
            .collect();

        if let Some(diff) = self.live_hunks() {
            for (_, hunk_rows) in diff.in_range(rows.clone()) {
                marked.extend(hunk_rows.start.max(rows.start)..hunk_rows.end.min(rows.end));
            }
        }

        marked
    }
}

/// The minimap edge-lane class for buffer `row`, or `None` when the line carries
/// no diff or diagnostic mark.
///
/// A diagnostic on the row wins over its diff status, mirroring the gutter. The
/// severity or diff status resolves to a class against `class_table`.
fn minimap_edge_class(
    diff: Option<&crate::diff_map::LiveHunks<'_>>,
    severity_map: &std::collections::BTreeMap<u32, lsp_types::DiagnosticSeverity>,
    class_table: &crate::minimap::ClassTable,
    row: u32,
) -> Option<u8> {
    use crate::{host::DiffStatus, minimap::EdgeClass};
    use lsp_types::DiagnosticSeverity;

    if let Some(severity) = severity_map.get(&row) {
        let kind = match *severity {
            DiagnosticSeverity::ERROR => EdgeClass::Error,
            DiagnosticSeverity::WARNING => EdgeClass::Warning,
            _ => EdgeClass::Info,
        };
        return Some(class_table.edge_class(kind));
    }

    match diff?.status_for_line(row) {
        DiffStatus::Added => Some(class_table.edge_class(EdgeClass::Added)),
        DiffStatus::Modified | DiffStatus::Moved => {
            Some(class_table.edge_class(EdgeClass::Modified))
        },
        DiffStatus::Unchanged => None,
    }
}

/// Convert the engine's [`crate::minimap::Run`]s to their `minimap_lines` wire form.
fn convert_minimap_runs(runs: Vec<crate::minimap::Run>) -> stoatty_protocol::command::LineSummary {
    runs.into_iter()
        .map(|run| stoatty_protocol::command::MinimapRun {
            start_col: run.start_col,
            len: run.len,
            class: run.class,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimap_syntax_version_separates_unparsed_from_parsed_at_zero() {
        let base = minimap_syntax_version(true, None, None);
        assert_eq!(
            base,
            minimap_syntax_version(true, None, None),
            "identical inputs leave the built rows alone"
        );
        assert_ne!(
            base,
            minimap_syntax_version(true, Some(0), None),
            "the first parse of an unedited buffer recolors the strip"
        );
        assert_ne!(
            minimap_syntax_version(true, Some(0), None),
            minimap_syntax_version(true, Some(1), None),
            "a reparse after an edit recolors the strip"
        );
        assert_ne!(
            base,
            minimap_syntax_version(true, None, Some(0)),
            "the first LSP semantic tokens for an unedited buffer recolor the strip"
        );
        assert_ne!(
            minimap_syntax_version(true, Some(0), Some(0)),
            minimap_syntax_version(true, Some(0), Some(1)),
            "LSP tokens for a newer buffer version recolor the strip"
        );
        assert_ne!(
            base,
            minimap_syntax_version(false, None, None),
            "toggling syntax highlighting recolors the strip"
        );
    }
}
