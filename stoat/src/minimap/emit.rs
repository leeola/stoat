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
                    !editor.diff_view
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
    // Where every row in range starts and ends, taken in two walks rather than
    // two descents per row of every span. A build sweeps thousands of rows a
    // tick, so the bucket loop below is left as arithmetic over these.
    let line_starts = {
        let points: Vec<stoat_text::Point> = (rows.start..=last_row)
            .map(|row| stoat_text::Point::new(row, 0))
            .collect();
        rope.points_to_offsets_batch(&points)
    };
    let line_lens = rope.line_lens_in_range(rows.start..last_row + 1);
    let line_end = |row: u32| -> usize {
        let index = (row - rows.start) as usize;
        line_starts[index] + line_lens[index] as usize
    };

    // The byte span of `rows`, so each channel resolves anchors only for the
    // tokens that can overlap it instead of the whole buffer.
    let byte_range = line_starts[0]..line_end(last_row);

    for highlights in [
        snapshot.semantic_token_highlights(),
        snapshot.lsp_token_highlights(),
    ] {
        let Some(channel) = highlights.get(&buffer_id) else {
            continue;
        };
        let bounds =
            channel.overlap_bounds(&byte_range, |anchor| buffer_snap.resolve_anchor(anchor));

        // Every surviving span's endpoints in one batch. Resolving them one at
        // a time descends from the root twice per span.
        let surviving: Vec<(u8, &_)> = channel
            .range(bounds)
            .filter_map(|span| match class_table.class_of(span.style) {
                0 => None,
                class => Some((class, span)),
            })
            .collect();
        let spans = {
            let anchors: Vec<stoat_text::Anchor> = surviving
                .iter()
                .flat_map(|(_, span)| [span.range.start, span.range.end])
                .collect();
            let offsets = buffer_snap.resolve_anchors_batch(&anchors);
            let points = rope.offsets_to_points_batch(&offsets);
            offsets
                .chunks_exact(2)
                .zip(points.chunks_exact(2))
                .map(|(offsets, points)| (offsets[0], offsets[1], points[0].row, points[1].row))
                .collect::<Vec<_>>()
        };

        for (&(class, _), &(start, end, first_row, last_span_row)) in surviving.iter().zip(&spans) {
            if start >= end {
                continue;
            }

            let start_row = first_row.max(rows.start);
            let end_row = last_span_row.min(last_row);
            for row in start_row..=end_row {
                let line_start = line_starts[(row - rows.start) as usize];
                let s = start.max(line_start);
                let e = end.min(line_end(row));
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
    severity_map: &'a crate::render::editor::RowSeverity,
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
        let mut marked = self.severity_map.rows_in(rows.clone());

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
    severity_map: &crate::render::editor::RowSeverity,
    class_table: &crate::minimap::ClassTable,
    row: u32,
) -> Option<u8> {
    use crate::{host::DiffStatus, minimap::EdgeClass};
    use lsp_types::DiagnosticSeverity;

    if let Some(severity) = severity_map.at(row) {
        let kind = match severity {
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
    use crate::{
        action_handlers, apc_emit,
        display_map::highlights::{HighlightStyleId, HighlightStyleInterner},
        test_fixture::{drain_apc, focused_editor_pane_area},
    };
    use ratatui::layout::Rect;
    use std::{path::PathBuf, sync::Arc};
    use stoat_action::OpenFile;
    use tokio::sync::mpsc::UnboundedReceiver;

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
    /// The rows a parse reports have to reach the strip that paints them.
    ///
    /// Splices cannot show this. A sweep queues nothing for a row whose
    /// summary is unchanged, so a scoped sweep and a full one emit the same
    /// frames and differ only in how many rows they re-summarize. What the
    /// scope does change is whether the sweep finishes inside one sync. Past
    /// one chunk's worth of rows a full sweep has to span several, which the
    /// strip reports as still pending.
    #[test]
    fn an_edit_sweeps_without_spilling_past_one_sync() {
        let mut h = Stoat::test();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        // More rows than one recolor chunk, so a full sweep cannot finish in
        // the single sync that follows the edit.
        let total = crate::minimap::RESYNC_CHUNK as usize + 500;
        let body: String = vec!["fn a() {}"; total].join("\n");
        let root = PathBuf::from("/minimap");
        let path = root.join("big.rs");
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.resize(80, 24);

        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        for _ in 0..100 {
            emit_minimap(&mut h.stoat);
            if !h.stoat.minimap_build_pending {
                break;
            }
        }
        assert!(
            !h.stoat.minimap_build_pending,
            "the fixture must settle its build and first full sweep",
        );

        // Insert a `let` on the second line, restaining that row alone.
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("a focused editor");
        {
            let buffer = h
                .stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer");
            buffer.write().expect("poisoned").edit(10..10, "let z = 1;");
        }

        // One settle only spawns the parse. Installing its result takes another
        // trip through the background drive.
        let target = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .read()
            .expect("poisoned")
            .snapshot
            .version;
        for _ in 0..10 {
            h.stoat.drive_background();
            h.settle();
            if h.stoat.active_workspace().buffers.syntax_version(buffer_id) == Some(target) {
                break;
            }
        }
        assert_eq!(
            h.stoat.active_workspace().buffers.syntax_version(buffer_id),
            Some(target),
            "the edit's reparse must land before the strip can sweep for it",
        );

        emit_minimap(&mut h.stoat);
        assert!(
            !h.stoat.minimap_build_pending,
            "a one-row recolor must not leave a multi-sync sweep outstanding",
        );
    }

    #[test]
    fn minimap_emits_declare_and_line_summaries() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/minimap");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.resize(120, 24);

        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        emit_minimap(&mut h.stoat);
        let first = drain_apc(&mut rx);
        assert!(
            first.iter().any(|cmd| matches!(cmd, Command::Minimap(_))),
            "the first frame declares the strip, got {first:?}"
        );
        assert!(
            first
                .iter()
                .any(|cmd| matches!(cmd, Command::MinimapLines(_))),
            "the first frame sends the initial line summaries, got {first:?}"
        );

        h.type_keys("i z");
        h.settle();
        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        emit_minimap(&mut h.stoat);
        let edited = drain_apc(&mut rx);
        assert!(
            edited
                .iter()
                .any(|cmd| matches!(cmd, Command::MinimapLines(_))),
            "an edit splices the changed line, got {edited:?}"
        );
    }

    #[test]
    fn a_multi_chunk_minimap_build_completes_over_idle_emits() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        // A file larger than one build chunk fills over several syncs.
        let total = 6000usize;
        let body: String = vec!["ln"; total].join("\n");
        let root = PathBuf::from("/minimap");
        let path = root.join("big.txt");
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.resize(80, 24);

        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        emit_minimap(&mut h.stoat);
        assert!(
            h.stoat.minimap_build_pending,
            "a multi-chunk file leaves the build pending after the first emit",
        );

        // Drive the build the way an idle frame tick does, gathering the lines
        // every emitted splice covers.
        let mut covered: Vec<u32> = Vec::new();
        for _ in 0..100 {
            for cmd in drain_apc(&mut rx) {
                if let Command::MinimapLines(lines) = cmd {
                    covered.extend(lines.start..lines.start + lines.lines.len() as u32);
                }
            }
            if !h.stoat.minimap_build_pending {
                break;
            }
            emit_minimap(&mut h.stoat);
        }

        assert!(
            !h.stoat.minimap_build_pending,
            "the build completes over successive emits",
        );
        covered.sort_unstable();
        assert_eq!(
            covered,
            (0..total as u32).collect::<Vec<_>>(),
            "the emitted splices cover every line exactly once",
        );
    }

    #[test]
    fn unfocused_pane_dims_its_minimap_declaration() {
        use crate::render::paint::{dim_rgb, style_rgb};
        use stoatty_protocol::command::{Command, MinimapCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
        // Wide enough that each vertical split clears the minimap min-width gate.
        h.resize(260, 24);

        let a = h.write_file("a.txt", "alpha\nbravo\ncharlie\n");
        let b = h.write_file("b.txt", "delta\necho\nfoxtrot\n");
        h.open_file(&a);
        h.type_action("SplitRight()");
        h.open_file(&b);
        h.settle();

        // 0.25 is the default ui.inactive_dim the test harness resolves.
        let dim = 0.25_f32;
        let bg = style_rgb(
            h.stoat
                .theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|s| s.bg),
        )
        .expect("test theme has an rgb background");
        let raw: Vec<[u8; 3]> = h.stoat.minimap_class_table.palette().to_vec();
        let dimmed: Vec<[u8; 3]> = raw.iter().map(|&c| dim_rgb(c, bg, dim)).collect();
        assert_ne!(raw, dimmed, "dim must actually change the palette");

        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        let minimaps: Vec<MinimapCommand> = drain_apc(&mut rx)
            .into_iter()
            .filter_map(|c| match c {
                Command::Minimap(m) => Some(m),
                _ => None,
            })
            .collect();

        let focused = minimaps
            .iter()
            .find(|m| m.palette == raw)
            .expect("the focused pane keeps the raw palette");
        let unfocused = minimaps
            .iter()
            .find(|m| m.palette == dimmed)
            .expect("the unfocused pane dims the palette");

        let [tr, tg, tb, ta] = focused.thumb;
        let [dr, dg, db] = dim_rgb([tr, tg, tb], bg, dim);
        assert_eq!(
            unfocused.thumb,
            [dr, dg, db, ta],
            "the unfocused thumb dims its rgb and keeps its alpha"
        );
        assert_eq!(
            unfocused.thumb_border,
            [dr, dg, db],
            "the unfocused thumb border dims"
        );

        // The blend is held across frames rather than redone on each one, so a
        // change to the dim has to reach the next frame instead of the first
        // blend standing for the rest of the session.
        let widened = 0.6_f32;
        h.stoat.settings.ui_inactive_dim = Some(widened.into());
        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        let rewidened: Vec<[u8; 3]> = raw.iter().map(|&c| dim_rgb(c, bg, widened)).collect();
        assert!(
            drain_apc(&mut rx)
                .into_iter()
                .any(|c| matches!(c, Command::Minimap(m) if m.palette == rewidened)),
            "a changed dim re-blends the held palette",
        );
    }

    #[test]
    fn single_minimap_mode_reserves_a_right_edge_band() {
        use stoat_config::MinimapMode;

        let mut h = Stoat::test();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Single);
        h.resize(200, 24);

        let a = h.write_file("a.txt", "alpha\nbravo\n");
        let b = h.write_file("b.txt", "delta\necho\n");
        h.open_file(&a);
        h.type_action("SplitRight()");
        h.open_file(&b);
        h.settle();
        let _ = h.stoat.render();

        let full = h.stoat.size();
        let band = h
            .stoat
            .single_minimap_rect
            .expect("single mode reserves a band");
        assert_eq!(
            band.x + band.width,
            full.width,
            "the band ends at the window right edge"
        );
        assert_eq!(band.y, full.y);
        assert_eq!(
            band.height,
            full.height - 1,
            "the band stops one row above the bottom status row"
        );
        assert!(band.width > 0, "the band has a real width");

        let focused_area = focused_editor_pane_area(&h);
        assert!(
            focused_area.x + focused_area.width <= band.x,
            "the focused pane stays left of the reserved band"
        );
        assert!(
            h.stoat
                .active_workspace()
                .editors
                .values()
                .all(|e| e.minimap_rect.is_none()),
            "single mode reserves no per-pane strips"
        );
    }

    #[test]
    fn single_minimap_band_leaves_the_bottom_status_row_full_width() {
        use stoat_config::MinimapMode;

        let mut h = Stoat::test();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Single);
        h.resize(200, 24);

        let a = h.write_file("a.txt", "alpha\nbravo\n");
        h.open_file(&a);
        h.settle();

        let size = h.stoat.size();
        let status_bg = h
            .stoat
            .theme
            .get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
            .bg
            .expect("theme has a focused status background");

        let buf = h.stoat.render();

        let band = h
            .stoat
            .single_minimap_rect
            .expect("single mode reserves a band");
        assert_eq!(
            band.height,
            size.height - 1,
            "the band stops one row above the bottom status row"
        );

        let bottom = size.height - 1;
        assert_eq!(
            buf[(size.width - 1, bottom)].bg,
            status_bg,
            "the focused status bar reaches the window's last column"
        );
    }

    #[test]
    fn single_minimap_mode_declares_one_strip_following_focus() {
        use stoat_config::MinimapMode;
        use stoatty_protocol::command::Command;

        let content_id = |h: &Stoat| -> u32 {
            let (editor_id, _) = h.focused_editor_ids().expect("focused editor");
            let buffer_id = h
                .active_workspace()
                .editors
                .get(editor_id)
                .expect("editor")
                .buffer_id;
            h.minimap_content
                .get(&(h.active_workspace, buffer_id))
                .expect("minimap content for the focused buffer")
                .content_id()
        };
        let single_strips = |cmds: &[Command]| -> Vec<u32> {
            cmds.iter()
                .filter_map(|c| match c {
                    Command::Minimap(m) if m.strip_id == u32::MAX => Some(m.content_id),
                    _ => None,
                })
                .collect()
        };

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Single);
        h.resize(200, 24);

        let a = h.write_file("a.txt", "alpha\nbravo\ncharlie\n");
        let b = h.write_file("b.txt", "delta\necho\nfoxtrot\n");
        h.open_file(&a);
        h.type_action("SplitRight()");
        h.open_file(&b);
        h.settle();

        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter()
                .all(|c| !matches!(c, Command::Minimap(m) if m.strip_id != u32::MAX)),
            "single mode declares no per-pane strips, got {cmds:?}"
        );
        let b_content = content_id(&h.stoat);
        assert_eq!(
            single_strips(&cmds).last().copied(),
            Some(b_content),
            "the single strip shows the focused buffer"
        );

        h.type_action("FocusNext()");
        h.settle();
        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        let cmds = drain_apc(&mut rx);
        let a_content = content_id(&h.stoat);
        assert_ne!(a_content, b_content, "the two panes show different buffers");
        assert_eq!(
            single_strips(&cmds).last().copied(),
            Some(a_content),
            "the strip redeclares for the newly focused buffer"
        );
    }

    #[test]
    fn minimap_drops_content_when_the_buffer_closes() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/minimap-drop");
        let a = root.join("a.txt");
        let b = root.join("b.txt");
        h.fake_fs().insert_file(&a, b"alpha\nbravo\n");
        h.fake_fs().insert_file(&b, b"charlie\ndelta\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: a });
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: b });
        h.settle();
        h.resize(80, 24);
        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        emit_minimap(&mut h.stoat);
        let _ = drain_apc(&mut rx);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::CloseBuffer);
        h.settle();
        let _ = h.stoat.render();
        emit_minimap(&mut h.stoat);
        let closed = drain_apc(&mut rx);
        assert!(
            closed
                .iter()
                .any(|cmd| matches!(cmd, Command::MinimapDrop(_))),
            "closing a buffer drops its minimap content, got {closed:?}"
        );
    }

    #[test]
    fn minimap_view_tracks_the_scroll_position() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/minimap-view");
        let path = root.join("a.txt");
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.resize(120, 24);

        let _ = h.stoat.render();
        apc_emit::emit_smooth_scroll(&mut h.stoat);
        let first = drain_apc(&mut rx);
        let top_at_origin = first.iter().find_map(|cmd| match cmd {
            Command::MinimapView(v) => Some(v.top_256),
            _ => None,
        });
        assert_eq!(
            top_at_origin,
            Some(0),
            "the origin thumb sits at line 0, got {first:?}"
        );

        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        {
            let editor = h
                .stoat
                .active_workspace_mut()
                .editors
                .get_mut(editor_id)
                .expect("editor");
            editor.scroll_row = 50;
            editor.scroll_offset = 50.0;
        }
        let _ = h.stoat.render();
        apc_emit::emit_smooth_scroll(&mut h.stoat);
        let scrolled = drain_apc(&mut rx);
        let top_after_scroll = scrolled.iter().find_map(|cmd| match cmd {
            Command::MinimapView(v) => Some(v.top_256),
            _ => None,
        });
        assert_eq!(
            top_after_scroll,
            Some(50 * 256),
            "the thumb tracks the scrolled top row, got {scrolled:?}"
        );
    }

    /// Several soft-wrapped display rows share one buffer line, and the strip
    /// draws one row per buffer line. An unconverted display top therefore
    /// overruns the strip's scrollable span near the bottom of the file, which
    /// pushes the thumb past the strip's bottom edge entirely.
    #[test]
    fn minimap_view_maps_wrapped_rows_to_buffer_lines() {
        use stoatty_protocol::command::Command;

        fn view_after_scroll(
            h: &mut crate::test_harness::TestHarness,
            rx: &mut UnboundedReceiver<Vec<u8>>,
            editor_id: EditorId,
            display_row: u32,
        ) -> (u32, u16) {
            let _ = drain_apc(rx);
            {
                let editor = h
                    .stoat
                    .active_workspace_mut()
                    .editors
                    .get_mut(editor_id)
                    .expect("editor");
                editor.scroll_row = display_row;
                editor.scroll_offset = display_row as f32;
            }
            let _ = h.stoat.render();
            apc_emit::emit_smooth_scroll(&mut h.stoat);
            let cmds = drain_apc(rx);
            cmds.iter()
                .rev()
                .find_map(|cmd| match cmd {
                    Command::MinimapView(v) => Some((v.top_256, v.visible_lines)),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("a minimap view emit, got {cmds:?}"))
        }

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/minimap-wrap");
        let path = root.join("wide.txt");
        // No trailing newline, so every buffer line is a long one and the wrap
        // ratio stays uniform across the file.
        let body = (0..60)
            .map(|i| format!("{i}{}", "w".repeat(150)))
            .collect::<Vec<_>>()
            .join("\n");
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        // Wide enough for the minimap strip to render at all, so the wrap comes
        // from the line length rather than a cramped pane.
        h.resize(120, 24);
        let _ = h.stoat.render();

        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        // The emit measures the window against the pool region, so the expected
        // spans below must be derived from that same height.
        let pool_rows = {
            let panes = apc_emit::editor_pool_panes(&h.stoat);
            let (_, _, region) = panes[0];
            region.height as u32
        };
        let (display_rows, buffer_lines, mid, bottom) = {
            let editor = h
                .stoat
                .active_workspace_mut()
                .editors
                .get_mut(editor_id)
                .expect("editor");
            let snapshot = editor.display_map.snapshot();
            let display_rows = snapshot.line_count();
            let line_at = |row: u32| {
                snapshot
                    .display_to_buffer(DisplayPoint::new(row, 0))
                    .expect("a text row")
                    .row
            };
            // The top display row, its buffer line, and the buffer lines the
            // pooled viewport covers from there.
            let window = |top_row: u32| {
                let last = (top_row + pool_rows - 1).min(display_rows - 1);
                (
                    top_row,
                    line_at(top_row),
                    line_at(last) - line_at(top_row) + 1,
                )
            };
            let max_scroll = display_rows
                .saturating_sub(1)
                .saturating_sub(pool_rows.saturating_sub(1));
            (
                display_rows,
                snapshot.buffer_line_count(),
                window(display_rows / 2),
                window(max_scroll),
            )
        };
        assert_eq!(
            display_rows,
            buffer_lines * 2,
            "the fixture must soft-wrap every line into exactly two display rows"
        );

        let (mid_row, mid_line, mid_visible) = mid;
        let emitted = view_after_scroll(&mut h, &mut rx, editor_id, mid_row);
        assert_eq!(
            emitted,
            (mid_line * 256, mid_visible as u16),
            "display row {mid_row} maps to buffer line {mid_line} over {mid_visible} lines"
        );

        let (max_row, max_line, max_visible) = bottom;
        let (top_256, visible) = view_after_scroll(&mut h, &mut rx, editor_id, max_row);
        assert_eq!(
            (top_256, visible),
            (max_line * 256, max_visible as u16),
            "the bottom-scrolled top maps to buffer line {max_line} over {max_visible} lines"
        );
        assert_eq!(
            top_256 / 256 + visible as u32,
            buffer_lines,
            "the bottom-scrolled window ends exactly at the last buffer line, so the thumb \
             stays on the strip"
        );
    }

    #[test]
    fn single_minimap_mode_syncs_content_for_every_visible_buffer() {
        use stoat_config::MinimapMode;
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Single);
        h.resize(200, 24);

        let a = h.write_file("a.txt", "alpha\nbravo\ncharlie\n");
        let b = h.write_file("b.txt", "delta\necho\nfoxtrot\n");
        h.open_file(&a);
        h.type_action("SplitRight()");
        h.open_file(&b);
        h.settle();
        let _ = h.stoat.render();

        let buffer_ids: Vec<BufferId> = {
            let ws = h.stoat.active_workspace();
            ws.panes
                .split_panes()
                .filter_map(|(_, pane)| match pane.view {
                    View::Editor(editor_id) => Some(ws.editors.get(editor_id)?.buffer_id),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(buffer_ids.len(), 2, "two visible editor buffers");
        let content_ids: Vec<u32> = buffer_ids
            .iter()
            .map(|&buffer_id| {
                h.stoat
                    .minimap_content
                    .get(&(h.stoat.active_workspace, buffer_id))
                    .expect("content for a visible buffer")
                    .content_id()
            })
            .collect();

        emit_minimap(&mut h.stoat);
        let synced: std::collections::HashSet<u32> = drain_apc(&mut rx)
            .iter()
            .filter_map(|c| match c {
                Command::MinimapLines(l) => Some(l.content_id),
                _ => None,
            })
            .collect();
        for id in content_ids {
            assert!(
                synced.contains(&id),
                "single mode syncs minimap_lines for every visible buffer; missing {id}, got {synced:?}"
            );
        }
    }

    #[test]
    fn single_minimap_view_follows_focus_and_rekeys_by_strip() {
        use stoat_config::MinimapMode;
        use stoatty_protocol::command::Command;

        let single_views = |cmds: &[Command]| -> Vec<u32> {
            cmds.iter()
                .filter_map(|c| match c {
                    Command::MinimapView(v) if v.strip_id == u32::MAX => Some(v.top_256),
                    _ => None,
                })
                .collect()
        };

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Single);
        h.resize(200, 24);

        let a = h.write_file("a.txt", "alpha\nbravo\ncharlie\n");
        let b = h.write_file("b.txt", "delta\necho\nfoxtrot\n");
        h.open_file(&a);
        h.type_action("SplitRight()");
        h.open_file(&b);
        h.settle();

        let _ = h.stoat.render();
        apc_emit::emit_smooth_scroll(&mut h.stoat);
        let cmds = drain_apc(&mut rx);
        assert_eq!(
            single_views(&cmds).last().copied(),
            Some(0),
            "single mode emits a view frame for strip u32::MAX at the origin"
        );
        assert!(
            cmds.iter()
                .all(|c| !matches!(c, Command::MinimapView(v) if v.strip_id != u32::MAX)),
            "single mode emits no per-pane view frames, got {cmds:?}"
        );

        // Focus the other pane, also at offset 0. Without keying the dedup by
        // strip and storing the pool, the shared strip would skip this frame as
        // an unmoved viewport. The pool change forces the re-emit.
        h.type_action("FocusNext()");
        h.settle();
        let _ = h.stoat.render();
        apc_emit::emit_smooth_scroll(&mut h.stoat);
        let cmds = drain_apc(&mut rx);
        assert_eq!(
            single_views(&cmds).last().copied(),
            Some(0),
            "focusing another pane at the same offset re-emits the strip view"
        );
    }

    #[test]
    fn a_centered_modal_undeclares_the_single_strip() {
        use stoat_action::OpenFileFinder;
        use stoat_config::MinimapMode;
        use stoatty_protocol::command::Command;

        let strip_declared = |cmds: &[Command]| -> bool {
            cmds.iter()
                .any(|c| matches!(c, Command::Minimap(m) if m.strip_id == u32::MAX))
        };

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Single);
        h.resize(200, 24);

        let a = h.write_file("a.txt", "alpha\nbravo\ncharlie\n");
        h.open_file(&a);
        h.settle();

        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        assert!(
            strip_declared(&drain_apc(&mut rx)),
            "single mode declares the strip while no modal is open"
        );

        action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
        h.settle();
        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        assert!(
            !strip_declared(&drain_apc(&mut rx)),
            "opening the finder undeclares the strip so the modal owns the right edge"
        );

        h.stoat.file_finder = None;
        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        assert!(
            strip_declared(&drain_apc(&mut rx)),
            "closing the finder redeclares the strip"
        );
    }

    /// The palette is the one centered modal that does not hide the strip.
    ///
    /// Its box is capped at 80 columns and centered, so it cannot reach a band
    /// that only exists from 108 columns up. Hiding the strip for it would
    /// cost the minimap on every palette open for no overlap.
    #[test]
    fn the_palette_leaves_the_single_strip_declared() {
        use stoat_action::OpenCommandPalette;
        use stoat_config::MinimapMode;
        use stoatty_protocol::command::Command;

        let strip_declared = |cmds: &[Command]| -> bool {
            cmds.iter()
                .any(|c| matches!(c, Command::Minimap(m) if m.strip_id == u32::MAX))
        };

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Single);
        h.resize(200, 24);

        let a = h.write_file("a.txt", "alpha\nbravo\ncharlie\n");
        h.open_file(&a);
        h.settle();

        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        assert!(strip_declared(&drain_apc(&mut rx)));

        action_handlers::dispatch(&mut h.stoat, &OpenCommandPalette);
        h.settle();
        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        assert!(
            h.stoat.command_palette.is_some(),
            "the palette must actually be open for this to mean anything"
        );
        assert!(
            strip_declared(&drain_apc(&mut rx)),
            "the strip survives the palette rather than vanishing under it"
        );

        h.stoat.command_palette = None;
        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        assert!(
            strip_declared(&drain_apc(&mut rx)),
            "and stays declared once the palette closes"
        );
    }

    #[test]
    fn the_hints_box_draws_over_the_single_strip() {
        use stoat_config::MinimapMode;
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Single);
        h.resize(200, 24);

        let a = h.write_file("a.txt", "alpha\nbravo\ncharlie\n");
        h.open_file(&a);
        h.settle();

        // The which-key box is standing hints, not a centered modal, so the strip
        // stays declared and the box draws on top of it.
        h.type_keys("space");
        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);

        let cmds = drain_apc(&mut rx);
        let strip_idx = cmds
            .iter()
            .position(|c| matches!(c, Command::Minimap(m) if m.strip_id == u32::MAX))
            .expect("the single strip stays declared under the hints box");
        let panel_idx = cmds
            .iter()
            .position(|c| matches!(c, Command::Panel(_)))
            .expect("the hints box emits a panel");
        assert!(
            strip_idx < panel_idx,
            "the strip declares before the hints panel so the panel occludes their overlap"
        );
    }

    #[test]
    fn minimap_marks_diff_and_diagnostic_lines() {
        use crate::minimap::EdgeClass;
        use lsp_types::DiagnosticSeverity;
        use stoatty_protocol::command::{Command, MinimapRun};

        fn diag(line: u32, severity: DiagnosticSeverity) -> lsp_types::Diagnostic {
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position { line, character: 0 },
                    end: lsp_types::Position { line, character: 1 },
                },
                severity: Some(severity),
                ..Default::default()
            }
        }

        // The leading run of buffer line `n` in the most recent emit.
        fn line_lead(cmds: &[Command], n: u32) -> Option<MinimapRun> {
            cmds.iter().rev().find_map(|cmd| match cmd {
                Command::MinimapLines(l) => {
                    let idx = n.checked_sub(l.start)? as usize;
                    l.lines.get(idx).and_then(|runs| runs.first().copied())
                },
                _ => None,
            })
        }

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/minimap-marks");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"keep\nnew\ntail\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;
        {
            let base = "keep\nold\ntail\n";
            let text = "keep\nnew\ntail\n";
            let dm = crate::diff_map::DiffMap::from_structural_changes(
                stoat_language::structural_diff::diff(base, text),
                Arc::new(base.to_string()),
                text,
            );
            h.stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer")
                .write()
                .expect("poisoned")
                .diff_map = Some(dm);
        }
        h.stoat
            .active_workspace_mut()
            .panes
            .resize(Rect::new(0, 0, 80, 24));

        let modified = h.stoat.minimap_class_table.edge_class(EdgeClass::Modified);
        let error = h.stoat.minimap_class_table.edge_class(EdgeClass::Error);

        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        emit_minimap(&mut h.stoat);
        let first = drain_apc(&mut rx);
        assert_eq!(
            line_lead(&first, 1).map(|r| r.class),
            Some(modified),
            "the modified line leads with the modified edge class, got {first:?}"
        );

        h.seed_diagnostics(path.clone(), vec![diag(1, DiagnosticSeverity::ERROR)]);
        let _ = h.stoat.render();
        emit_minimap(&mut h.stoat);
        let errored = drain_apc(&mut rx);
        assert_eq!(
            line_lead(&errored, 1).map(|r| r.class),
            Some(error),
            "an error overrides the diff mark, got {errored:?}"
        );

        h.seed_diagnostics(path.clone(), vec![]);
        let _ = h.stoat.render();
        emit_minimap(&mut h.stoat);
        let cleared = drain_apc(&mut rx);
        assert_eq!(
            line_lead(&cleared, 1).map(|r| r.class),
            Some(modified),
            "clearing the diagnostic reverts to the modified mark, got {cleared:?}"
        );
        let touched: Vec<u32> = cleared
            .iter()
            .filter_map(|c| match c {
                Command::MinimapLines(l) => Some(l.start),
                _ => None,
            })
            .collect();
        assert_eq!(touched, vec![1], "only the formerly-marked line re-splices");

        // A recomputed diff moves the hunk to line 2 with the buffer untouched,
        // so line 2 has to pick up a mark it has never carried before.
        {
            let base = "keep\nnew\nold\n";
            let text = "keep\nnew\ntail\n";
            let dm = crate::diff_map::DiffMap::from_structural_changes(
                stoat_language::structural_diff::diff(base, text),
                Arc::new(base.to_string()),
                text,
            );
            h.stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer")
                .write()
                .expect("poisoned")
                .diff_map = Some(dm);
        }
        let _ = h.stoat.render();
        emit_minimap(&mut h.stoat);
        let moved = drain_apc(&mut rx);
        assert_eq!(
            line_lead(&moved, 2).map(|r| r.class),
            Some(modified),
            "the newly-differing line takes the modified mark, got {moved:?}"
        );
        assert_eq!(
            line_lead(&moved, 1).map(|r| r.start_col),
            Some(2),
            "line 1 matches the base again, so its lane is empty, got {moved:?}"
        );

        h.seed_diagnostics(path, vec![diag(0, DiagnosticSeverity::ERROR)]);
        let _ = h.stoat.render();
        emit_minimap(&mut h.stoat);
        let on_clean = drain_apc(&mut rx);
        assert_eq!(
            line_lead(&on_clean, 0).map(|r| r.class),
            Some(error),
            "a diagnostic marks a line no hunk covers, got {on_clean:?}"
        );
    }

    /// The style id and interner for the first `THEME_KEYS` scope, as the
    /// production paths hand them to a token channel.
    ///
    /// Tokens must intern through the shared table, since that is what the
    /// minimap's class lookup is keyed by.
    fn first_scope_style(stoat: &Stoat) -> (HighlightStyleId, Arc<HighlightStyleInterner>) {
        let style = stoat
            .syntax_styles
            .id_for_highlight(stoat_language::HighlightId(0))
            .expect("the first theme key resolves");
        (style, stoat.syntax_styles.interner.clone())
    }

    #[test]
    fn minimap_colors_align_past_a_leading_tab() {
        use crate::display_map::highlights::SemanticTokenHighlight;
        use std::sync::Arc;
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/minimap-tab");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"\tfoo\nbar\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let (editor_id, buffer_id) = {
            let ids = h.stoat.focused_editor_ids().expect("editor");
            (ids.0, ids.1)
        };

        // Give the token a real syntax scope so it maps to a class.
        let (style, interner) = first_scope_style(&h.stoat);
        let expected_class = h.stoat.minimap_class_table.class_of(style);
        assert_ne!(
            expected_class, 0,
            "the test style must map to a syntax class"
        );

        let range = {
            let shared = h
                .stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer");
            let snap = shared.read().expect("poisoned").snapshot.clone();
            snap.anchor_at(1, Bias::Right)..snap.anchor_at(4, Bias::Left)
        };
        let tokens: Arc<[SemanticTokenHighlight]> =
            Arc::from(vec![SemanticTokenHighlight { range, style }]);
        h.stoat.active_workspace_mut().editors[editor_id]
            .display_map
            .set_semantic_token_highlights(buffer_id, tokens, interner);

        h.stoat
            .active_workspace_mut()
            .panes
            .resize(Rect::new(0, 0, 80, 24));
        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        emit_minimap(&mut h.stoat);
        let cmds = drain_apc(&mut rx);

        let line0 = cmds
            .iter()
            .rev()
            .find_map(|cmd| match cmd {
                Command::MinimapLines(l) if l.start == 0 => l.lines.first().cloned(),
                _ => None,
            })
            .expect("line 0 summary");

        // The tab expands content to column 4, where the token's colored run
        // begins. The old display-chunk mapping placed the token past the raw
        // line's bytes, dropping the color.
        assert_eq!(
            line0.first().map(|run| (run.start_col, run.class)),
            Some((4, expected_class)),
            "the run starts at the tab-expanded column in the syntax class, got {line0:?}"
        );
    }

    #[test]
    fn minimap_recolors_on_syntax_toggle() {
        use crate::display_map::highlights::SemanticTokenHighlight;
        use std::sync::Arc;
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/minimap-toggle");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"foo\nbar\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let (editor_id, buffer_id) = {
            let ids = h.stoat.focused_editor_ids().expect("editor");
            (ids.0, ids.1)
        };

        let (style, interner) = first_scope_style(&h.stoat);
        let colored_class = h.stoat.minimap_class_table.class_of(style);
        assert_ne!(
            colored_class, 0,
            "the test style must map to a syntax class"
        );

        let range = {
            let shared = h
                .stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer");
            let snap = shared.read().expect("poisoned").snapshot.clone();
            snap.anchor_at(0, Bias::Right)..snap.anchor_at(3, Bias::Left)
        };
        let tokens: Arc<[SemanticTokenHighlight]> =
            Arc::from(vec![SemanticTokenHighlight { range, style }]);
        h.stoat.active_workspace_mut().editors[editor_id]
            .display_map
            .set_semantic_token_highlights(buffer_id, tokens, interner);
        h.stoat
            .active_workspace_mut()
            .panes
            .resize(Rect::new(0, 0, 80, 24));

        let line0_class = |cmds: &[Command]| {
            cmds.iter().rev().find_map(|cmd| match cmd {
                Command::MinimapLines(l) if l.start == 0 => l
                    .lines
                    .first()
                    .and_then(|runs| runs.first())
                    .map(|r| r.class),
                _ => None,
            })
        };

        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        emit_minimap(&mut h.stoat);
        let colored = drain_apc(&mut rx);
        assert_eq!(
            line0_class(&colored),
            Some(colored_class),
            "line 0 is colored under syntax highlighting, got {colored:?}"
        );

        // Toggling syntax off re-summarizes the built lines monochrome, with no
        // buffer edit.
        h.stoat.syntax_highlight = false;
        let _ = h.stoat.render();
        emit_minimap(&mut h.stoat);
        let mono = drain_apc(&mut rx);
        assert_eq!(
            line0_class(&mono),
            Some(0),
            "the toggle recolors line 0 monochrome, got {mono:?}"
        );
    }

    /// Several spans across several rows, including one that runs past a line
    /// ending and one outside the asked-for range, so the bucketing has to clip
    /// each piece to its own row and drop what the range does not reach.
    #[test]
    fn minimap_line_tokens_buckets_each_span_into_its_rows() {
        use crate::display_map::highlights::SemanticTokenHighlight;
        use std::sync::Arc;

        let mut h = Stoat::test();
        let root = PathBuf::from("/minimap-buckets");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"aaaa\nbbbb\ncccc\ndddd\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let (editor_id, buffer_id) = {
            let ids = h.stoat.focused_editor_ids().expect("editor");
            (ids.0, ids.1)
        };
        let (style, interner) = first_scope_style(&h.stoat);
        let class = h.stoat.minimap_class_table.class_of(style);
        assert_ne!(class, 0, "the test style must map to a syntax class");

        // Rows are five bytes each. One span covers the tail of row 0 through
        // the head of row 2, one sits inside row 3, and one covers row 5, which
        // the file does not reach.
        let spans: Arc<[SemanticTokenHighlight]> = {
            let shared = h
                .stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer");
            let snap = shared.read().expect("poisoned").snapshot.clone();
            let at = |start: usize, end: usize| SemanticTokenHighlight {
                range: snap.anchor_at(start, Bias::Right)..snap.anchor_at(end, Bias::Left),
                style,
            };
            Arc::from(vec![at(2, 12), at(16, 18), at(25, 27)])
        };
        h.stoat.active_workspace_mut().editors[editor_id]
            .display_map
            .set_semantic_token_highlights(buffer_id, spans, interner);

        let snapshot = h.stoat.active_workspace_mut().editors[editor_id]
            .display_map
            .snapshot();
        let tokens = minimap_line_tokens(
            &snapshot,
            buffer_id,
            h.stoat.syntax_highlight,
            &h.stoat.minimap_class_table,
            0..3,
        );

        let row = |row: u32| -> Vec<(usize, usize, u8)> {
            tokens.get(&row).map_or_else(Vec::new, |line| {
                line.iter()
                    .map(|token| (token.range.start, token.range.end, token.class))
                    .collect()
            })
        };
        assert_eq!(row(0), vec![(2, 4, class)], "clipped to the line's own end");
        assert_eq!(row(1), vec![(0, 4, class)], "the whole line between");
        assert_eq!(
            row(2),
            vec![(0, 2, class)],
            "clipped to where the span ends"
        );
        assert_eq!(row(3), Vec::new(), "outside the rows asked about");
        assert_eq!(tokens.len(), 3, "and no other row carries anything");
    }

    /// A theme switch keeps the minimap's syntax highlighting.
    ///
    /// Classifying by resolved color meant a switch left the token's stale
    /// foreground matching nothing in the new table, so it classified 0 and the
    /// token vanished from the strip. Classifying by style id is immune,
    /// because the id names the scope rather than a color.
    #[test]
    fn minimap_keeps_its_classes_across_a_theme_switch() {
        use crate::display_map::highlights::SemanticTokenHighlight;
        use std::sync::Arc;

        let mut h = Stoat::test();
        let root = PathBuf::from("/minimap-theme");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"foo\nbar\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let (editor_id, buffer_id) = {
            let ids = h.stoat.focused_editor_ids().expect("editor");
            (ids.0, ids.1)
        };

        let (style, interner) = first_scope_style(&h.stoat);
        let expected = h.stoat.minimap_class_table.class_of(style);
        assert_ne!(expected, 0, "the test style must map to a syntax class");

        let range = {
            let shared = h
                .stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer");
            let snap = shared.read().expect("poisoned").snapshot.clone();
            snap.anchor_at(0, Bias::Right)..snap.anchor_at(3, Bias::Left)
        };
        let tokens: Arc<[SemanticTokenHighlight]> =
            Arc::from(vec![SemanticTokenHighlight { range, style }]);
        h.stoat.active_workspace_mut().editors[editor_id]
            .display_map
            .set_semantic_token_highlights(buffer_id, tokens, interner);

        let row0_classes = |stoat: &mut Stoat| {
            let snapshot = stoat.active_workspace_mut().editors[editor_id]
                .display_map
                .snapshot();
            minimap_line_tokens(
                &snapshot,
                buffer_id,
                stoat.syntax_highlight,
                &stoat.minimap_class_table,
                0..1,
            )
            .remove(&0)
            .unwrap_or_default()
            .iter()
            .map(|token| token.class)
            .collect::<Vec<_>>()
        };
        assert_eq!(row0_classes(&mut h.stoat), vec![expected]);

        action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::SetTheme {
                name: "gruvbox-light".to_string(),
            },
        );

        let after = h.stoat.minimap_class_table.class_of(style);
        assert_ne!(after, 0, "the scope still has a class under the new theme");
        assert_eq!(
            row0_classes(&mut h.stoat),
            vec![after],
            "the token keeps its class instead of dropping off the strip"
        );
    }
}
