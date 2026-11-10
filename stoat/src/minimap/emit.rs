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
    diff_changes::DiffChangesId,
    display_map::{DisplayPoint, DisplaySnapshot},
    editor_state::EditorId,
    minimap::{
        density::{self, Density, Tier},
        diff_changes::{self, DiffChangesStrip},
        BuildWindow, EdgeSource, RowMark, LINES_PER_CELL,
    },
    pane::View,
    render::{paint, review, SINGLE_MINIMAP_STRIP_ID},
    theme::Theme,
    workspace::WorkspaceId,
};
use ratatui::layout::Rect;
use std::{
    cell::OnceCell,
    collections::hash_map::{DefaultHasher, Entry},
    hash::{Hash, Hasher},
    ops::Range,
    sync::Arc,
};
use stoat_config::MinimapMode;
use stoat_text::Bias;

/// Assign a `content_id` to each visible split-editor buffer that a minimap
/// strip renders, and to the active workspace's changes view strip while an
/// changes view shows, so the declare widget reads the id while the frame
/// paints.
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
        diff_changes_strips,
        minimap_next_content_id,
        executor,
        ..
    } = stoat;
    let ws = &workspaces[ws_id];

    for (_, pane) in ws.panes.split_panes() {
        let editor_id = match pane.view {
            View::Editor(editor_id) => editor_id,
            View::DiffChanges(_) => {
                if let Entry::Vacant(slot) = diff_changes_strips.entry(ws_id) {
                    slot.insert(DiffChangesStrip::new(*minimap_next_content_id));
                    *minimap_next_content_id += 1;
                }
                continue;
            },
            _ => continue,
        };
        let Some(editor) = ws.editors.get(editor_id) else {
            continue;
        };
        if let Entry::Vacant(slot) = minimap_content.entry((ws_id, editor.buffer_id)) {
            slot.insert(crate::minimap::MinimapContent::new(
                *minimap_next_content_id,
                executor.clone(),
            ));
            *minimap_next_content_id += 1;
        }
    }
}

/// The protocol version that decodes a weighted run. An older terminal drops
/// the frame whole, so the splice goes as `minimap_lines` and its runs draw at
/// the flat strength that terminal already renders.
pub(super) const WEIGHTED_RUN_PROTOCOL: u32 = 4;

/// The palettes a frame's minimap strips declare beside the class table's own.
///
/// A plain strip whose pane does not fade declares the class table's palette
/// as it is. A strip whose pane fades declares its palette blended toward the
/// background at the frame's fade, the deeper of the inactive dim and a boxed
/// modal's dim. The strip of a split diff or a changes view declares its scroll
/// overview, the class table's row marks on the window ground.
///
/// Every strip of a kind declares the same palette, so the frame builds them
/// once and keeps them until the theme or the dim moves. See
/// [`Stoat::refresh_minimap_palettes`].
pub(crate) struct MinimapPalettes {
    theme: Arc<Theme>,
    dim: f32,
    /// The class table's palette at the frame's fade. `None` when no fade
    /// applies or the background is not RGB, where a fading strip declares the
    /// palette undimmed.
    pub(crate) dimmed: Option<Vec<[u8; 3]>>,
    /// The palette a scroll overview declares, as
    /// [`review::diff_overview_palette`] builds it over the class table's
    /// palette on the window ground.
    pub(crate) overview: Vec<[u8; 3]>,
    /// [`Self::overview`] at the frame's fade, `None` where [`Self::dimmed`] is.
    pub(crate) overview_dimmed: Option<Vec<[u8; 3]>>,
}

impl MinimapPalettes {
    /// Build the palettes for `table` under `theme` at the fade `dim`.
    pub(crate) fn new(table: &crate::minimap::ClassTable, theme: &Arc<Theme>, dim: f32) -> Self {
        let dim_bg = (dim > 0.0)
            .then(|| {
                paint::style_rgb(
                    theme
                        .try_get(crate::theme::scope::UI_BACKGROUND)
                        .and_then(|s| s.bg),
                )
            })
            .flatten();
        let blend = |palette: &[[u8; 3]]| {
            dim_bg.map(|bg| {
                palette
                    .iter()
                    .map(|&c| paint::dim_rgb(c, bg, dim))
                    .collect()
            })
        };

        let dimmed = blend(table.palette());
        let ground = crate::minimap::color_to_rgb(crate::render::themed_bg(theme));
        let overview = review::diff_overview_palette(table, table.palette(), ground);
        let overview_dimmed = dim_bg.zip(dimmed.as_deref()).map(|(bg, dimmed)| {
            review::diff_overview_palette(table, dimmed, paint::dim_rgb(ground, bg, dim))
        });

        Self {
            theme: theme.clone(),
            dim,
            dimmed,
            overview,
            overview_dimmed,
        }
    }

    /// Whether these palettes were built from these inputs, so a frame that
    /// passes the same ones keeps them.
    pub(crate) fn built_from(&self, theme: &Arc<Theme>, dim: f32) -> bool {
        Arc::ptr_eq(&self.theme, theme) && self.dim == dim
    }
}

/// Sync each visible minimap strip's buffer and drain its summary changes to
/// the terminal, retiring content for buffers that closed.
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
    // Single mode has no per-pane rects, so it syncs every visible editor's
    // buffer, keeping all of them warm for instant focus switches.
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
                let included = single || editor.minimap_rect.is_some();
                included.then_some((editor.buffer_id, editor_id))
            })
            .collect()
    };

    // Per-pane mode places a changes view's thumb on its own strip where the
    // paint reserved one. Single mode places the focused changes view's thumb
    // on the band.
    let diff_changes_views: Vec<(DiffChangesId, Option<u32>)> = {
        let ws = &stoat.workspaces[ws_id];
        let focused = ws.focused_diff_changes_id();
        ws.panes
            .split_panes()
            .filter_map(|(_, pane)| {
                let View::DiffChanges(id) = pane.view else {
                    return None;
                };
                let state = ws.diff_changes_views.get(id)?;
                let strip_id = match single {
                    true => (focused == Some(id)).then_some(SINGLE_MINIMAP_STRIP_ID),
                    false => state.minimap_rect.is_some().then_some(pane.index),
                };
                Some((id, strip_id))
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
    if !diff_changes_views.is_empty() {
        diff_changes::sync_diff_changes_strip(stoat, ws_id, &mut out);
        stoat.minimap_build_pending |= stoat
            .diff_changes_strips
            .get(&ws_id)
            .is_some_and(DiffChangesStrip::build_pending);
    }
    for (id, strip_id) in diff_changes_views {
        if let Some(strip_id) = strip_id {
            diff_changes::emit_diff_changes_thumb(stoat, ws_id, id, strip_id, &mut out);
        }
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
    // A parked stack keeps the strip, so the flip back finds the lines of
    // every excerpt that did not move still held and sends none of them.
    let ws = &stoat.workspaces[ws_id];
    let stackless = ws.diff_changes_views.is_empty() && ws.parked_diff_changes.is_none();
    if stackless && let Some(strip) = stoat.diff_changes_strips.remove(&ws_id) {
        stoatty_protocol::command::encode_minimap_drop_into(
            &mut out,
            &stoatty_protocol::command::MinimapDropCommand {
                content_id: strip.content_id(),
            },
        );
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
    let single_strip = match stoat.minimap_mode() {
        MinimapMode::Single => Some(stoat.single_minimap_rect),
        _ => None,
    };
    let thumb_min_rows = stoat.minimap_thumb_min_rows();
    let (snapshot, diff_version, diag_version, severity_map, density, window) =
        match stoat.workspaces[ws_id].editors.get_mut(editor_id) {
            Some(editor) => {
                let snapshot = editor.display_map.snapshot();
                let diff_version = editor.display_map.diff_version();
                let (diag_version, severity_map) = editor
                    .gutter_severity_cache
                    .as_ref()
                    .map(|cache| (cache.version, cache.map.clone()))
                    .unwrap_or_default();
                let strip = single_strip.unwrap_or(editor.minimap_rect);
                let viewport = editor.viewport_rows.unwrap_or(0);
                let view = minimap_view_window(
                    &snapshot,
                    editor.scroll_row as f32 + editor.scroll_frac,
                    viewport,
                );
                let density = strip_density(
                    snapshot.buffer_line_count(),
                    strip,
                    u32::from(view.1),
                    thumb_min_rows,
                );
                editor.minimap_density = (density.tier != Tier::Line).then_some(density);
                let window = minimap_build_window(view, viewport, strip, density);
                (
                    snapshot,
                    diff_version,
                    diag_version,
                    severity_map,
                    density,
                    window,
                )
            },
            None => return,
        };
    let comment_prefixes = stoat.workspaces[ws_id]
        .buffers
        .language_for(buffer_id)
        .map_or(&[][..], |language| language.line_comments);
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
    let classes = stoat.minimap_class_table.class_version;
    let syntax_version = minimap_syntax_version(
        stoat.syntax_highlight,
        classes,
        buffer_syntax_version,
        lsp_token_version,
    );
    let syntax_other_version =
        minimap_syntax_other_version(stoat.syntax_highlight, classes, lsp_token_version);

    let syntax_on = stoat.syntax_highlight;
    // Read before the store is borrowed below, which holds `stoat` for the rest
    // of the function.
    let weighted = stoat.stoatty_protocol >= WEIGHTED_RUN_PROTOCOL;
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
        comment_prefixes,
        class_table,
        density,
        window,
    );

    if let Some(lines) = content.take_len() {
        stoatty_protocol::command::encode_minimap_len_into(
            out,
            &stoatty_protocol::command::MinimapLenCommand { content_id, lines },
        );
    }
    for splice in content.take_queued() {
        let command = stoatty_protocol::command::MinimapLinesCommand {
            content_id,
            start: splice.start,
            removed: splice.removed,
            lines: splice.lines.into_iter().map(convert_minimap_runs).collect(),
        };
        if weighted {
            stoatty_protocol::command::encode_minimap_runs_into(out, &command);
        } else {
            stoatty_protocol::command::encode_minimap_lines_into(out, &command);
        }
    }
}

/// The rows a strip builds, which are the editor's view at rest padded by two
/// strip spans on each side.
///
/// A strip draws its height in cells times [`LINES_PER_CELL`] lines, and that
/// span rides at most one span past the view in either direction, so one span
/// of padding holds every line it draws. The second span is scroll margin, so
/// a page or two of scroll lands on lines already sent. The view is read at
/// rest rather than mid-glide, so a jump builds where it lands.
///
/// `view` is the viewport's top buffer line and its height in lines, as
/// [`minimap_view_window`] reads them at rest for a viewport of `viewport`
/// rows. `strip` is the strip's rect when known. A strip shorter than the
/// viewport still pads by the viewport, so a first frame before the rect is
/// laid out builds enough.
///
/// Under a `density` past one line per row the window is in strip rows. It
/// holds the row of the view's top line and the rows the view's lines take,
/// padded by the same strip spans.
fn minimap_build_window(
    view: (f32, u16),
    viewport: u32,
    strip: Option<Rect>,
    density: Density,
) -> BuildWindow {
    let (top, visible) = view;
    let cells = strip.map_or(0, |rect| u32::from(rect.height)).max(viewport);
    let pad = 2 * cells * u32::from(LINES_PER_CELL);
    let top = density::row_of(top as u32, density.lines_per_row);
    let visible = u32::from(visible).div_ceil(density.lines_per_row);
    BuildWindow {
        rows: top.saturating_sub(pad)..top.saturating_add(visible + pad),
        from: top,
    }
}

/// The density of a strip of rect `strip` over a file of `file_lines` lines,
/// under a viewport of `viewport_lines` lines.
///
/// A strip with no rect laid out yet holds one line per row, so a first frame
/// before layout builds lines.
fn strip_density(
    file_lines: u32,
    strip: Option<Rect>,
    viewport_lines: u32,
    thumb_min_rows: u32,
) -> Density {
    match strip.filter(|rect| rect.height > 0) {
        Some(rect) => density::density(
            file_lines,
            u32::from(rect.height) * u32::from(LINES_PER_CELL),
            viewport_lines,
            thumb_min_rows,
        ),
        None => Density::LINE,
    }
}

/// Content version of a minimap strip's coloring, hashing the inputs whose
/// change must re-summarize every row already built.
///
/// `classes` is the class table's
/// [`class_version`](crate::minimap::ClassTable::class_version), which moves
/// when a theme switch changes which scopes carry a class.
///
/// The parse and LSP versions stay [`Option`]s rather than collapsing to a
/// sentinel. Both count *buffer* versions and a freshly opened buffer sits at
/// version 0, so flattening `None` to 0 would make "not parsed yet" and "parsed
/// the untouched file" hash alike. Any buffer past the sync-parse cap summarizes
/// its rows before its first parse lands, and that collision would leave them
/// monochrome until an unrelated edit re-summarized them.
fn minimap_syntax_version(
    syntax_on: bool,
    classes: u64,
    parse: Option<u64>,
    lsp: Option<u64>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    syntax_on.hash(&mut hasher);
    classes.hash(&mut hasher);
    parse.hash(&mut hasher);
    lsp.hash(&mut hasher);
    hasher.finish()
}

/// The part of [`minimap_syntax_version`] the parse does not contribute.
///
/// Only the parse reports which rows its tokens changed, so the strip needs to
/// tell a bump it has row information for from one it does not. A change here
/// puts the recolor sweep back to covering every built row.
fn minimap_syntax_other_version(syntax_on: bool, classes: u64, lsp: Option<u64>) -> u64 {
    let mut hasher = DefaultHasher::new();
    syntax_on.hash(&mut hasher);
    classes.hash(&mut hasher);
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
///
/// A row gathers its tokens in the editor's precedence order: the tree-sitter
/// channel, then the LSP channel, each in channel index order, which is the
/// slot order the editor's highlight keys take. Each row then goes through
/// [`crate::minimap::resolve_overlaps`], so its tokens come out disjoint and
/// each byte keeps the token the editor paints, an escape inside a string and
/// an LSP token over a tree-sitter one included.
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
                .as_chunks::<2>()
                .0
                .iter()
                .zip(points.as_chunks::<2>().0.iter())
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
        crate::minimap::resolve_overlaps(tokens);
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
    /// not put. Held once for the pass, since `mark_of` runs per row and the
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
    fn mark_of(&self, row: u32) -> RowMark {
        minimap_row_mark(self.live_hunks(), self.severity_map, self.class_table, row)
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

/// The minimap mark of buffer `row`, its change lane and its diagnostic tick.
///
/// The lane follows the row's diff status and the tick its diagnostic
/// severity, each resolved to a class against `class_table`, so a row with
/// both shows both.
fn minimap_row_mark(
    diff: Option<&crate::diff_map::LiveHunks<'_>>,
    severity_map: &crate::render::editor::RowSeverity,
    class_table: &crate::minimap::ClassTable,
    row: u32,
) -> RowMark {
    use crate::{host::DiffStatus, minimap::EdgeClass};
    use lsp_types::DiagnosticSeverity;

    let status = diff.map_or(DiffStatus::Unchanged, |diff| diff.status_for_line(row));
    let lane = match status {
        DiffStatus::Added => Some(EdgeClass::Added),
        DiffStatus::Modified | DiffStatus::Moved => Some(EdgeClass::Modified),
        DiffStatus::Unchanged => None,
    };
    let tick = severity_map.at(row).map(|severity| match severity {
        DiagnosticSeverity::ERROR => EdgeClass::Error,
        DiagnosticSeverity::WARNING => EdgeClass::Warning,
        _ => EdgeClass::Info,
    });

    RowMark {
        lane: lane.map(|kind| class_table.edge_class(kind)),
        tick: tick.map(|kind| class_table.edge_class(kind)),
    }
}

/// Convert the engine's [`crate::minimap::Run`]s to their wire form.
pub(super) fn convert_minimap_runs(
    runs: crate::minimap::LineRuns,
) -> stoatty_protocol::command::LineSummary {
    runs.as_slice()
        .iter()
        .map(|run| stoatty_protocol::command::MinimapRun {
            start_col: run.start_col,
            len: run.len,
            class: run.class,
            weight: run.weight,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        action_handlers, apc_emit,
        display_map::highlights::{HighlightStyleId, HighlightStyleInterner},
        minimap::SYNC_SLICE,
        test_fixture::{drain_apc, focused_editor_pane_area},
    };
    use ratatui::layout::Rect;
    use std::{path::PathBuf, sync::Arc, time::Duration};
    use stoat_action::OpenFile;
    use tokio::sync::mpsc::UnboundedReceiver;

    #[test]
    fn minimap_syntax_version_separates_unparsed_from_parsed_at_zero() {
        let base = minimap_syntax_version(true, 0, None, None);
        assert_eq!(
            base,
            minimap_syntax_version(true, 0, None, None),
            "identical inputs leave the built rows alone"
        );
        assert_ne!(
            base,
            minimap_syntax_version(true, 0, Some(0), None),
            "the first parse of an unedited buffer recolors the strip"
        );
        assert_ne!(
            minimap_syntax_version(true, 0, Some(0), None),
            minimap_syntax_version(true, 0, Some(1), None),
            "a reparse after an edit recolors the strip"
        );
        assert_ne!(
            base,
            minimap_syntax_version(true, 0, None, Some(0)),
            "the first LSP semantic tokens for an unedited buffer recolor the strip"
        );
        assert_ne!(
            minimap_syntax_version(true, 0, Some(0), Some(0)),
            minimap_syntax_version(true, 0, Some(0), Some(1)),
            "LSP tokens for a newer buffer version recolor the strip"
        );
        assert_ne!(
            base,
            minimap_syntax_version(false, 0, None, None),
            "toggling syntax highlighting recolors the strip"
        );
        assert_ne!(
            base,
            minimap_syntax_version(true, 1, None, None),
            "a class table that gives other scopes a class recolors the strip"
        );
    }

    /// Give every strip a zero budget, so each sync covers one slice per stage.
    ///
    /// The harness clock moves only when a test moves it, so the default budget
    /// never ends a sync. A test that needs a file to take several syncs sets
    /// this after the render that creates the strip.
    fn one_slice_per_sync(stoat: &mut Stoat) {
        for content in stoat.minimap_content.values_mut() {
            content.budget = Duration::ZERO;
        }
    }

    /// The rows a parse reports have to reach the strip that paints them.
    ///
    /// Splices do not show this. A sweep queues nothing for a row whose
    /// summary is unchanged, so a scoped sweep and a full one emit the same
    /// frames and differ only in how many rows they re-summarize. What the
    /// scope does change is whether the sweep finishes inside one sync. At one
    /// slice per sync, a full sweep of a file of several slices outruns one
    /// sync, which the strip reports as still pending.
    #[test]
    fn an_edit_sweeps_without_spilling_past_one_sync() {
        let mut h = Stoat::test();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        // Several slices, so a full sweep outruns one sync, which the last leg
        // asserts rather than assumes.
        let total = 4 * SYNC_SLICE as usize;
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
        one_slice_per_sync(&mut h.stoat);
        for _ in 0..400 {
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

        // The scope is only visible against a full sweep this fixture cannot
        // finish in one sync. Turning the highlighting off and on again asks
        // for one, since a change with no rows behind it covers everything.
        h.stoat.syntax_highlight = false;
        for _ in 0..400 {
            emit_minimap(&mut h.stoat);
            if !h.stoat.minimap_build_pending {
                break;
            }
        }
        assert!(
            !h.stoat.minimap_build_pending,
            "the fixture must settle before the full sweep it is measured against",
        );

        h.stoat.syntax_highlight = true;
        emit_minimap(&mut h.stoat);
        assert!(
            h.stoat.minimap_build_pending,
            "a full sweep must outrun one sync, or the scoped sweep proves nothing",
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

    /// A file longer than the strip's window sends its length and then only
    /// the window's lines, over as many idle emits as the budget takes. A
    /// scroll moves the window, and the emits after it cover only the lines
    /// the new window adds.
    #[test]
    fn a_large_file_builds_only_the_window_its_view_needs() {
        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        // Eight slices long, at a height whose window takes two slices, so a
        // window fills over two syncs at one slice each.
        let total = 8 * SYNC_SLICE;
        let body: String = vec!["ln"; total as usize].join("\n");
        let root = PathBuf::from("/minimap");
        let path = root.join("big.txt");
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.resize(80, 60);
        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;

        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        one_slice_per_sync(&mut h.stoat);
        let (first_lens, first_rows) = build_until_idle(&mut h, &mut rx);
        let first = expected_window(&mut h, editor_id, total);

        {
            let editor = h
                .stoat
                .active_workspace_mut()
                .editors
                .get_mut(editor_id)
                .expect("editor");
            editor.scroll_row = total - 50;
            editor.scroll_offset = (total - 50) as f32;
        }
        let _ = h.stoat.render();
        let (second_lens, second_rows) = build_until_idle(&mut h, &mut rx);
        let second = expected_window(&mut h, editor_id, total);

        assert!(
            first.start == 0 && first.end < second.start && second.end == total,
            "two windows of a file longer than both, apart: {first:?} {second:?}",
        );
        assert_eq!(
            [(first_lens, first_rows), (second_lens, second_rows)],
            [(vec![total], first.collect()), (vec![], second.collect())],
            "the length goes once, and each window's lines go exactly once",
        );
    }

    /// `body` open in a 120 by 24 window under a terminal that reads every
    /// protocol, so its strip is a 23-cell band of 138 rows over a 23-line
    /// viewport.
    fn open_in_band(body: &str) -> (crate::test_harness::TestHarness, UnboundedReceiver<Vec<u8>>) {
        let mut h = Stoat::test();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;

        let root = PathBuf::from("/minimap-bands");
        let path = root.join("numbered.txt");
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.resize(120, 24);
        (h, rx)
    }

    /// Emit until the minimap build settles, replay what it sent into `store`
    /// as the terminal applies it, and answer the store lengths it sent.
    fn settle_store(
        h: &mut crate::test_harness::TestHarness,
        rx: &mut UnboundedReceiver<Vec<u8>>,
        store: &mut Vec<Vec<stoatty_protocol::command::MinimapRun>>,
    ) -> Vec<u32> {
        use stoatty_protocol::command::Command;

        let _ = h.stoat.render();
        let mut lens = Vec::new();
        for _ in 0..400 {
            emit_minimap(&mut h.stoat);
            for cmd in drain_apc(rx) {
                match cmd {
                    Command::MinimapLen(len) => {
                        store.resize(len.lines as usize, Vec::new());
                        lens.push(len.lines);
                    },
                    Command::MinimapLines(splice) => {
                        let start = splice.start as usize;
                        let end = (start + splice.removed as usize).min(store.len());
                        store.splice(start..end, splice.lines.iter().map(|line| line.to_vec()));
                    },
                    _ => {},
                }
            }
            if !h.stoat.minimap_build_pending {
                break;
            }
        }
        lens
    }

    /// The runs a strip shows for a plain line `text` that carries no mark.
    fn plain_runs(text: &str) -> Vec<stoatty_protocol::command::MinimapRun> {
        convert_minimap_runs(crate::minimap::summarize_line(text, &[], RowMark::NONE)).to_vec()
    }

    /// `count` numbered lines, `line 0` on, with the rows of `blank` left
    /// empty.
    fn numbered(count: usize, blank: Range<usize>) -> Vec<String> {
        (0..count)
            .map(|i| match blank.contains(&i) {
                true => String::new(),
                false => format!("line {i}"),
            })
            .collect()
    }

    /// 1,200 lines fold 9 to a row on a 138-row band under the default Thumb
    /// Minimum's cap of 11, so the store holds 134 rows. Each row is the
    /// summary of one line of its band from the content column, and the thumb
    /// covers the 23-line viewport over 9 lines a row.
    #[test]
    fn a_medium_file_fits_the_strip_as_bands() {
        use stoatty_protocol::command::Command;

        let lines = numbered(1_200, 0..0);
        let (mut h, mut rx) = open_in_band(&lines.join("\n"));
        let mut store = Vec::new();
        let lens = settle_store(&mut h, &mut rx, &mut store);

        let _ = h.stoat.render();
        apc_emit::emit_smooth_scroll(&mut h.stoat);
        let visible = drain_apc(&mut rx)
            .into_iter()
            .rev()
            .find_map(|cmd| match cmd {
                Command::MinimapView(view) => Some(view.visible_256),
                _ => None,
            });
        let rows_show_their_bands = store.iter().enumerate().all(|(row, runs)| {
            (row * 9..(row * 9 + 9).min(1_200)).any(|line| plain_runs(&lines[line]) == *runs)
        });

        assert_eq!(
            (lens, store.len(), rows_show_their_bands, visible),
            (vec![134], 134, true, Some(23 * 256 / 9)),
        );
    }

    /// Lines appended past a multiple of the fold add a row, so the store is
    /// sized again before the rows go out. 1,207 lines at 9 a row take 135
    /// rows, one more than 1,200 lines do.
    #[test]
    fn an_edit_that_crosses_a_row_multiple_rebands_the_file() {
        let body = numbered(1_200, 0..0).join("\n");
        let (mut h, mut rx) = open_in_band(&body);
        let mut store = Vec::new();
        settle_store(&mut h, &mut rx, &mut store);

        h.edit_focused(body.len()..body.len(), &"\nmore".repeat(7));
        let lens = settle_store(&mut h, &mut rx, &mut store);

        assert_eq!(
            (lens, store.len(), store.last().map(Vec::is_empty)),
            (vec![135], 135, Some(false)),
        );
    }

    /// A band whose lines are mostly blank draws an empty row. A twenty-line
    /// blank gap from line 500 fills the band of row 56, and leaves the band of
    /// row 57 two code lines of nine, under the share a row needs.
    #[test]
    fn a_blank_band_draws_an_empty_row() {
        let (mut h, mut rx) = open_in_band(&numbered(1_200, 500..520).join("\n"));
        let mut store = Vec::new();
        settle_store(&mut h, &mut rx, &mut store);

        assert_eq!(
            (
                store.len(),
                store[55..59].iter().map(Vec::is_empty).collect::<Vec<_>>()
            ),
            (134, vec![false, true, true, false]),
        );
    }

    /// Emit until the minimap build settles, the way idle frame ticks drive
    /// it, and return the store lengths sent and the rows the splices covered.
    fn build_until_idle(
        h: &mut crate::test_harness::TestHarness,
        rx: &mut UnboundedReceiver<Vec<u8>>,
    ) -> (Vec<u32>, Vec<u32>) {
        use stoatty_protocol::command::Command;

        let mut lens = Vec::new();
        let mut rows = Vec::new();
        for emit in 0..400 {
            emit_minimap(&mut h.stoat);
            for cmd in drain_apc(rx) {
                match cmd {
                    Command::MinimapLen(len) => lens.push(len.lines),
                    Command::MinimapLines(lines) => {
                        rows.extend(lines.start..lines.start + lines.lines.len() as u32)
                    },
                    _ => {},
                }
            }
            assert!(
                emit > 0 || h.stoat.minimap_build_pending,
                "a window of two slices outlasts its first emit",
            );
            if !h.stoat.minimap_build_pending {
                break;
            }
        }
        rows.sort_unstable();
        (lens, rows)
    }

    /// The rows the focused editor's strip builds, cut to the file.
    fn expected_window(
        h: &mut crate::test_harness::TestHarness,
        editor_id: EditorId,
        total: u32,
    ) -> Range<u32> {
        let single = h.stoat.single_minimap_rect;
        let single_mode = h.stoat.minimap_mode() == MinimapMode::Single;
        let thumb_min_rows = h.stoat.minimap_thumb_min_rows();
        let editor = h
            .stoat
            .active_workspace_mut()
            .editors
            .get_mut(editor_id)
            .expect("editor");
        let strip = if single_mode {
            single
        } else {
            editor.minimap_rect
        };
        let snapshot = editor.display_map.snapshot();
        let viewport = editor.viewport_rows.unwrap_or(0);
        let view = minimap_view_window(
            &snapshot,
            editor.scroll_row as f32 + editor.scroll_frac,
            viewport,
        );
        let density = strip_density(
            snapshot.buffer_line_count(),
            strip,
            u32::from(view.1),
            thumb_min_rows,
        );
        let window = minimap_build_window(view, viewport, strip, density);
        window.rows.start..window.rows.end.min(total)
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

        let dim = h.stoat.painted_inactive_dim();
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
            unfocused.thumb_border, None,
            "the unfocused thumb carries no outline to dim"
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

    /// A strip declares the class table's whole palette to every terminal. The
    /// palette fits the 64-entry cap of a terminal before protocol 13, so that
    /// terminal reads it too.
    #[test]
    fn a_strip_declares_the_palette_its_terminal_reads() {
        use stoatty_protocol::command::Command;

        let declared = |protocol: u32| {
            let mut h = Stoat::test();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            h.stoat.set_apc_tx(tx);
            h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
            h.stoat.stoatty_protocol = protocol;
            h.resize(160, 24);
            let a = h.write_file("a.txt", "alpha\n");
            h.open_file(&a);
            h.settle();

            let _ = h.stoat.render();
            apc_emit::emit_apc_scene(&mut h.stoat);
            let palettes: Vec<Vec<[u8; 3]>> = drain_apc(&mut rx)
                .into_iter()
                .filter_map(|c| match c {
                    Command::Minimap(m) => Some(m.palette),
                    _ => None,
                })
                .collect();
            (palettes, h.stoat.minimap_class_table.palette().to_vec())
        };

        let (current, full) = declared(stoatty_protocol::PROTOCOL_VERSION);
        let (older, _) = declared(12);
        assert_eq!(
            (current, older, full.len() <= 64),
            (vec![full.clone()], vec![full], true)
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

    /// Under islands the band sits inside the window's right margin, so the
    /// window reads margin, panes, band, margin.
    #[test]
    fn under_islands_the_band_sits_inside_the_right_margin() {
        use stoat_config::MinimapMode;

        let mut h = Stoat::test();
        h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Single);
        h.resize(200, 24);
        let _ = h.stoat.render();

        let band = h
            .stoat
            .single_minimap_rect
            .expect("single mode reserves a band");
        let panes = h.stoat.layout_size();
        assert_eq!(
            [panes.x, panes.x + panes.width, band.x + band.width],
            [1, band.x, 199],
            "a margin, the panes up to the band, then the band one column short of the edge",
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

    /// The window pads the view by two strip spans on each side. A strip draws
    /// its cells times six lines and rides at most one span past the view,
    /// so the window holds every line it draws, plus a span of scroll margin.
    /// A strip shorter than the viewport pads by the viewport.
    #[test]
    fn the_build_window_pads_the_view_by_two_strip_spans() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/minimap-window");
        let path = root.join("a.txt");
        h.fake_fs()
            .insert_file(&path, vec!["ln"; 3000].join("\n").as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.resize(80, 40);
        let _ = h.stoat.render();

        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        let editor = h
            .stoat
            .active_workspace_mut()
            .editors
            .get_mut(editor_id)
            .expect("editor");
        editor.scroll_row = 1000;
        editor.scroll_frac = 0.0;
        editor.viewport_rows = Some(30);
        let snapshot = editor.display_map.snapshot();
        let view = minimap_view_window(&snapshot, 1000.0, 30);
        let strip = |height| Some(Rect::new(0, 0, 8, height));
        let window = |rows| BuildWindow { rows, from: 1000 };

        assert_eq!(
            [
                minimap_build_window(view, 30, None, Density::LINE),
                minimap_build_window(view, 30, strip(40), Density::LINE),
                minimap_build_window(view, 30, strip(20), Density::LINE),
            ],
            [window(640..1390), window(520..1510), window(640..1390)],
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
        ) -> (u32, u32) {
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
                    Command::MinimapView(v) => Some((v.top_256, v.visible_256)),
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
            (mid_line * 256, mid_visible * 256),
            "display row {mid_row} maps to buffer line {mid_line} over {mid_visible} lines"
        );

        let (max_row, max_line, max_visible) = bottom;
        let (top_256, visible) = view_after_scroll(&mut h, &mut rx, editor_id, max_row);
        assert_eq!(
            (top_256, visible),
            (max_line * 256, max_visible * 256),
            "the bottom-scrolled top maps to buffer line {max_line} over {max_visible} lines"
        );
        assert_eq!(
            (top_256 + visible) / 256,
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

    /// The palette takes the list-modal width, so on a wide window its box
    /// reaches the band and it undeclares the strip as the finder does.
    #[test]
    fn a_centered_modal_undeclares_the_single_strip() {
        use stoat_action::{OpenCommandPalette, OpenFileFinder};
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

        action_handlers::dispatch(&mut h.stoat, &OpenCommandPalette);
        h.settle();
        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        assert!(
            !strip_declared(&drain_apc(&mut rx)),
            "opening the palette undeclares it too"
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
        // The status row's mode chip is a panel too, one row tall.
        let panel_idx = cmds
            .iter()
            .position(|c| matches!(c, Command::Panel(panel) if panel.height > 1))
            .expect("the hints box emits a panel");
        assert!(
            strip_idx < panel_idx,
            "the strip declares before the hints panel so the panel occludes their overlap"
        );
    }

    /// A plain text file whose line 1 the diff marks modified, open in an 80x24
    /// pane, with the receiver of the frames the session sends and the file's
    /// path.
    fn modified_line_fixture() -> (
        crate::test_harness::TestHarness,
        UnboundedReceiver<Vec<u8>>,
        PathBuf,
    ) {
        let mut h = Stoat::test();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
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
        (h, rx, path)
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

        // The runs of buffer line `n` in the most recent emit that carries it.
        fn line_runs(cmds: &[Command], n: u32) -> Option<Vec<MinimapRun>> {
            cmds.iter().rev().find_map(|cmd| match cmd {
                Command::MinimapLines(l) => {
                    let idx = n.checked_sub(l.start)? as usize;
                    l.lines.get(idx).map(|runs| runs.to_vec())
                },
                _ => None,
            })
        }

        fn run(start_col: u8, len: u8, class: u8, weight: u8) -> MinimapRun {
            MinimapRun {
                start_col,
                len,
                class,
                weight,
            }
        }

        let (mut h, mut rx, path) = modified_line_fixture();
        h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
        let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;

        let modified = h.stoat.minimap_class_table.edge_class(EdgeClass::Modified);
        let error = h.stoat.minimap_class_table.edge_class(EdgeClass::Error);
        let lane = [run(0, 6, modified, 255), run(8, 112, modified, 41)];
        let tick = run(114, 6, error, 255);
        let (keep, new, tail) = (run(8, 4, 0, 190), run(8, 3, 0, 190), run(8, 4, 0, 190));

        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        emit_minimap(&mut h.stoat);
        let first = drain_apc(&mut rx);
        assert_eq!(
            line_runs(&first, 1),
            Some([&lane[..], &[new]].concat()),
            "the modified line takes the modified lane and tint, got {first:?}"
        );

        h.seed_diagnostics(path.clone(), vec![diag(1, DiagnosticSeverity::ERROR)]);
        let _ = h.stoat.render();
        emit_minimap(&mut h.stoat);
        let errored = drain_apc(&mut rx);
        assert_eq!(
            line_runs(&errored, 1),
            Some([&lane[..], &[new, tick]].concat()),
            "an error ticks the right edge beside the diff lane, got {errored:?}"
        );

        h.seed_diagnostics(path.clone(), vec![]);
        let _ = h.stoat.render();
        emit_minimap(&mut h.stoat);
        let cleared = drain_apc(&mut rx);
        assert_eq!(
            line_runs(&cleared, 1),
            Some([&lane[..], &[new]].concat()),
            "clearing the diagnostic drops the tick, got {cleared:?}"
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
            (line_runs(&moved, 2), line_runs(&moved, 1)),
            (Some([&lane[..], &[tail]].concat()), Some(vec![new])),
            "the newly-differing line takes the modified lane, and line 1 matches \
             the base again, so it carries its content alone, got {moved:?}"
        );

        h.seed_diagnostics(path, vec![diag(0, DiagnosticSeverity::ERROR)]);
        let _ = h.stoat.render();
        emit_minimap(&mut h.stoat);
        let on_clean = drain_apc(&mut rx);
        assert_eq!(
            line_runs(&on_clean, 0),
            Some(vec![keep, tick]),
            "a diagnostic ticks a line no hunk covers, got {on_clean:?}"
        );
    }

    /// The run classes of buffer line `n` in the most recent emit that carries
    /// it.
    fn line_classes(cmds: &[stoatty_protocol::command::Command], n: u32) -> Option<Vec<u8>> {
        use stoatty_protocol::command::Command;

        cmds.iter().rev().find_map(|cmd| match cmd {
            Command::MinimapLines(l) => {
                let idx = n.checked_sub(l.start)? as usize;
                l.lines
                    .get(idx)
                    .map(|runs| runs.iter().map(|run| run.class).collect())
            },
            _ => None,
        })
    }

    /// A changed row leads with its lane at full strength and a 16 percent tint
    /// of the lane's color across its content, which keeps the plain syntax
    /// classes. An unchanged row carries its content alone.
    #[test]
    fn a_diff_row_carries_its_tint() {
        use crate::minimap::EdgeClass;
        use stoatty_protocol::command::{Command, MinimapRun};

        let (mut h, mut rx, _) = modified_line_fixture();
        let lane = h.stoat.minimap_class_table.edge_class(EdgeClass::Modified);
        h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
        let cmds = emit_frame(&mut h, &mut rx);
        let runs = cmds.iter().rev().find_map(|cmd| match cmd {
            Command::MinimapLines(l) => l.lines.get(1usize.checked_sub(l.start as usize)?).cloned(),
            _ => None,
        });
        let runs = runs.expect("the modified line goes out");

        assert_eq!(
            (
                line_classes(&cmds, 0),
                runs[..2].to_vec(),
                runs[2..]
                    .iter()
                    .all(|run| run.class == 0 && run.start_col >= 8)
            ),
            (
                Some(vec![0]),
                vec![
                    MinimapRun {
                        start_col: 0,
                        len: 6,
                        class: lane,
                        weight: 255,
                    },
                    MinimapRun {
                        start_col: 8,
                        len: 112,
                        class: lane,
                        weight: 41,
                    },
                ],
                true,
            ),
            "the modified line takes its lane, then its tint, then its plain content",
        );
    }

    /// Paint a frame, send its scene and its minimap content, and return every
    /// command the terminal receives.
    fn emit_frame(
        h: &mut crate::test_harness::TestHarness,
        rx: &mut UnboundedReceiver<Vec<u8>>,
    ) -> Vec<stoatty_protocol::command::Command> {
        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        emit_minimap(&mut h.stoat);
        drain_apc(rx)
    }

    /// The single band follows its editor into the diff view. It declares the
    /// scroll overview of the class table's palette, again at a theme switch,
    /// and the buffer's lines sync with their lanes and tints.
    ///
    /// A dial step sends neither the band nor a line, since neither the
    /// overview nor the summaries hold a dial.
    #[test]
    fn a_diff_view_band_declares_the_overview_palette() {
        use crate::{minimap::EdgeClass, render::SINGLE_MINIMAP_STRIP_ID};
        use stoat_action::SetTheme;
        use stoatty_protocol::command::Command;

        let band = |cmds: &[Command]| {
            cmds.iter().find_map(|c| match c {
                Command::Minimap(m) if m.strip_id == SINGLE_MINIMAP_STRIP_ID => {
                    Some(m.palette.clone())
                },
                _ => None,
            })
        };
        let splices = |cmds: &[Command]| {
            cmds.iter()
                .filter(|c| matches!(c, Command::MinimapLines(_)))
                .count()
        };

        let (mut h, mut rx, _) = modified_line_fixture();
        h.stoat.stoatty_protocol = stoatty_protocol::PROTOCOL_VERSION;
        h.stoat.diff_tint = 2;
        // The diff view's hints box shares the overlay lane with the band, so a
        // toggle mark that the box repaints sends the band again.
        h.stoat.screen_hints_hidden = true;
        h.resize(120, 24);
        action_handlers::review::enter_diff_view(&mut h.stoat);
        let diff = emit_frame(&mut h, &mut rx);
        h.stoat.diff_tint = 1;
        let stepped = emit_frame(&mut h, &mut rx);
        h.stoat.diff_syntax = false;
        let flat = emit_frame(&mut h, &mut rx);

        let overview = |h: &crate::test_harness::TestHarness| {
            let table = &h.stoat.minimap_class_table;
            let ground = crate::minimap::color_to_rgb(crate::render::themed_bg(&h.stoat.theme));
            review::diff_overview_palette(table, table.palette(), ground)
        };
        let table = &h.stoat.minimap_class_table;
        let first_overview = overview(&h);
        assert_ne!(
            first_overview,
            table.palette(),
            "the overview moves the palette"
        );
        assert_eq!(
            (
                band(&diff),
                line_classes(&diff, 1),
                band(&stepped),
                band(&flat),
                (splices(&stepped), splices(&flat)),
            ),
            (
                Some(first_overview.clone()),
                Some(vec![
                    table.edge_class(EdgeClass::Modified),
                    table.edge_class(EdgeClass::Modified),
                    0,
                ]),
                None,
                None,
                (0, 0),
            ),
            "the band declares the overview over the diff view's synced lines, and \
             a tint step and a syntax toggle send neither the band nor a line",
        );

        action_handlers::dispatch(
            &mut h.stoat,
            &SetTheme {
                name: "gruvbox-light".to_string(),
            },
        );
        let themed = emit_frame(&mut h, &mut rx);
        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(false);
        let plain = emit_frame(&mut h, &mut rx);

        let themed_palette = overview(&h);
        assert_ne!(
            themed_palette, first_overview,
            "the theme moves the palette"
        );
        assert_eq!(
            (band(&themed), band(&plain)),
            (
                Some(themed_palette),
                Some(h.stoat.minimap_class_table.palette().to_vec())
            ),
            "a theme switch declares the overview under the new theme, and the \
             class table's own palette once the view is plain",
        );
    }

    /// A strip declares the palette for its view and its focus. A diff view's
    /// overview takes the class table's palette with its text classes on the
    /// window ground, and an unfocused strip takes its palette at the inactive
    /// dim. A terminal before protocol 13 receives the same palettes.
    #[test]
    fn a_strip_declares_the_palette_for_its_view_and_focus() {
        use stoatty_protocol::command::Command;

        // A session per protocol, because a session's terminal changes protocol
        // only across a handshake, which the pane cache keys on.
        for protocol in [stoatty_protocol::PROTOCOL_VERSION, 12] {
            let mut h = Stoat::test();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            h.stoat.set_apc_tx(tx);
            h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
            h.stoat.stoatty_protocol = protocol;
            // Wide enough that each vertical split clears the minimap min-width
            // gate.
            h.resize(260, 24);
            let a = h.write_file("a.txt", "alpha\n");
            let b = h.write_file("b.txt", "bravo\n");
            // Sorted, because the pair says nothing about which pane declares
            // first. Each frame holds one diff pane and one plain pane under
            // opposite focus, so the four palettes keep the pairs apart.
            let mut declared = |h: &mut crate::test_harness::TestHarness| {
                let mut palettes: Vec<Vec<[u8; 3]>> = emit_frame(h, &mut rx)
                    .into_iter()
                    .filter_map(|c| match c {
                        Command::Minimap(m) => Some(m.palette),
                        _ => None,
                    })
                    .collect();
                palettes.sort();
                palettes.dedup();
                palettes
            };

            h.open_file(&a);
            h.type_action("SplitRight()");
            h.open_file(&b);
            h.type_action("FocusNext()");
            action_handlers::focused_editor_mut(&mut h.stoat)
                .expect("editor")
                .set_diff_view(true);
            let diff_focused = declared(&mut h);

            // The keys the harness finds for FocusNext leave a diff view rather
            // than move the focus, so the focus moves through the pane tree.
            h.stoat.active_workspace_mut().panes.focus_next();
            let diff_unfocused = declared(&mut h);

            let table = &h.stoat.minimap_class_table;
            let bg = paint::style_rgb(
                h.stoat
                    .theme
                    .try_get(crate::theme::scope::UI_BACKGROUND)
                    .and_then(|s| s.bg),
            )
            .expect("the test theme has an rgb background");
            let dim = h.stoat.painted_inactive_dim();
            let dimmed = |palette: &[[u8; 3]]| -> Vec<[u8; 3]> {
                palette
                    .iter()
                    .map(|&c| paint::dim_rgb(c, bg, dim))
                    .collect()
            };
            let plain = table.palette().to_vec();
            let ground = crate::minimap::color_to_rgb(crate::render::themed_bg(&h.stoat.theme));
            let overview = review::diff_overview_palette(table, &plain, ground);
            let dimmed_overview = review::diff_overview_palette(
                table,
                &dimmed(&plain),
                paint::dim_rgb(ground, bg, dim),
            );
            let sorted = |mut pair: Vec<Vec<[u8; 3]>>| {
                pair.sort();
                pair
            };
            assert_eq!(
                (diff_focused, diff_unfocused),
                (
                    sorted(vec![overview, dimmed(&plain)]),
                    sorted(vec![dimmed_overview, plain]),
                ),
                "at protocol {protocol}",
            );
        }
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

        // The tab expands content to column 12, where the token's colored run
        // starts. A token placed by display chunk lands past the raw line's
        // bytes and loses its color.
        assert_eq!(
            line0.first().map(|run| (run.start_col, run.class)),
            Some((12, expected_class)),
            "the run starts at the tab-expanded column in the syntax class, got {line0:?}"
        );
    }

    /// A strip over a buffer whose row 0 starts with a word in the first syntax
    /// scope, built and drained once.
    fn colored_word_strip() -> (crate::test_harness::TestHarness, UnboundedReceiver<Vec<u8>>) {
        use crate::display_map::highlights::SemanticTokenHighlight;

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

        assert_eq!(
            synced_row0_class(&mut h, &mut rx),
            Some(colored_class),
            "line 0 is colored under syntax highlighting"
        );
        (h, rx)
    }

    /// Render a frame and sync the strips, and answer the class of the first
    /// run in the last splice that resends row 0.
    fn synced_row0_class(
        h: &mut crate::test_harness::TestHarness,
        rx: &mut UnboundedReceiver<Vec<u8>>,
    ) -> Option<u8> {
        use stoatty_protocol::command::Command;

        let _ = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        emit_minimap(&mut h.stoat);
        drain_apc(rx).iter().rev().find_map(|cmd| match cmd {
            Command::MinimapLines(l) if l.start == 0 => l
                .lines
                .first()
                .and_then(|runs| runs.first())
                .map(|r| r.class),
            _ => None,
        })
    }

    #[test]
    fn minimap_recolors_on_syntax_toggle() {
        let (mut h, mut rx) = colored_word_strip();

        // Toggling syntax off re-summarizes the built lines monochrome, with no
        // buffer edit.
        h.stoat.syntax_highlight = false;
        assert_eq!(
            synced_row0_class(&mut h, &mut rx),
            Some(0),
            "the toggle recolors line 0 monochrome"
        );
    }

    /// A class table that gives the word's scope no class recolors the row, as
    /// a theme switch that takes the scope's foreground away does.
    #[test]
    fn minimap_recolors_on_a_class_table_change() {
        use crate::{minimap::ClassTable, theme::Theme};

        let (mut h, mut rx) = colored_word_strip();

        h.stoat.minimap_class_table = ClassTable::from_theme(&Theme::empty());
        assert_eq!(
            synced_row0_class(&mut h, &mut rx),
            Some(0),
            "the new table recolors line 0 with no buffer edit"
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

    /// The editor paints an LSP token over the tree-sitter token under it, so
    /// the strip carries the LSP token's class alone over that word.
    #[test]
    fn an_lsp_token_over_a_syntax_token_colors_the_strip_as_the_text() {
        use crate::display_map::highlights::{BufferSemanticTokens, SemanticTokenHighlight};

        let mut h = Stoat::test();
        let root = PathBuf::from("/minimap-overlap");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"word rest\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let (editor_id, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
        let style = |id| {
            h.stoat
                .syntax_styles
                .id_for_highlight(stoat_language::HighlightId(id))
                .expect("the theme key resolves")
        };
        let (syntax, lsp) = (style(0), style(2));
        let interner = h.stoat.syntax_styles.interner.clone();
        let classes = (
            h.stoat.minimap_class_table.class_of(syntax),
            h.stoat.minimap_class_table.class_of(lsp),
        );
        assert!(
            classes.0 != 0 && classes.1 != 0 && classes.0 != classes.1,
            "two distinct syntax classes, got {classes:?}",
        );

        let snap = {
            let shared = h
                .stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer");
            shared.read().expect("poisoned").snapshot.clone()
        };
        let word = |style| -> Arc<[SemanticTokenHighlight]> {
            Arc::from(vec![SemanticTokenHighlight {
                range: snap.anchor_at(0, Bias::Right)..snap.anchor_at(4, Bias::Left),
                style,
            }])
        };
        let display_map = &mut h.stoat.active_workspace_mut().editors[editor_id].display_map;
        display_map.set_semantic_token_highlights(buffer_id, word(syntax), interner.clone());
        display_map.set_lsp_token_channel(
            buffer_id,
            BufferSemanticTokens::with_resolved_ends(word(lsp), interner, &[4]),
        );
        let snapshot = display_map.snapshot();

        let tokens = minimap_line_tokens(
            &snapshot,
            buffer_id,
            h.stoat.syntax_highlight,
            &h.stoat.minimap_class_table,
            0..1,
        );
        let row: Vec<(usize, usize, u8)> = tokens[&0]
            .iter()
            .map(|token| (token.range.start, token.range.end, token.class))
            .collect();
        assert_eq!(row, vec![(0, 4, classes.1)]);
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
