//! What the app sends stoatty each frame, over the APC wire.
//!
//! Running under stoatty, the app draws twice. The cell grid goes out as
//! ordinary terminal writes, and everything the cells cannot express -- the
//! smooth-scroll pools, the aux windows and their content, the theme's default
//! colors -- goes out as APC command frames from here.
//!
//! Almost everything here is versioned rather than resent. A pool page, a window's
//! content, and an input view each hash to a version, and a frame that hashes the
//! same as the last one emits nothing. That is what keeps a per-frame protocol
//! affordable: a still screen costs a few comparisons, and a scroll costs only
//! the pages that moved.

use crate::{
    action_handlers::view,
    app::{modal_split_percent, modal_zoom_steps, ModalKind, Stoat},
    commit_list::Preview,
    display_map::{DisplayPoint, DisplaySnapshot, PaintVersion},
    editor_state::{EditorId, ScrollGlide},
    input_view::InputView,
    minimap::emit::minimap_view_window,
    pane::{FocusTarget, Placement, View},
    render::{
        hover::HoverFrame,
        review::DiffDials,
        undercurl::UndercurlBatch,
        walkthrough::{self, Spotlight},
    },
    term_session::{TermId, TermSession},
    workspace::{diff, Workspace, WorkspaceId},
};
use ratatui::{buffer::Buffer, layout::Rect};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::Arc,
};
use stoat_config::{LineNumbers, WrapMode};
use stoat_widgets::{
    pool::{self, MinimapWindowInputs, PageVersions, Refill, SmoothScrollState},
    ApcScene,
};
use stoatty_protocol::command::{PoolAnchorCommand, PoolKind, PoolRegionCommand};

/// The protocol version that reads a decorations-only page fill. An older
/// terminal ignores the marker and lays the runs after it on the live grid, so
/// a cursor line change refills its pages whole.
const FILL_DECORATIONS_PROTOCOL: u32 = 5;

/// The protocol version that composites a terminal-kind pool at the terminal
/// font size and reports its grid. Under an older terminal a terminal pane
/// draws on the live grid at the editor's font size.
const TERMINAL_POOL_PROTOCOL: u32 = 9;

/// A split pane showing a terminal or an agent, as the terminal-kind pool that
/// draws it at the terminal font size.
///
/// The focus and the dim travel with the region because the page paints both,
/// the cursor cell and the fade an unfocused pane takes, as the live grid does.
struct TerminalPoolPane {
    term_id: TermId,
    region: PoolRegionCommand,
    is_focused: bool,
    /// How far the page blends toward the background. Zero for the focused
    /// pane.
    dim: f32,
}

/// The surface whose pages a pane pool holds.
///
/// A pane pool takes its pane's index as its id, and the root pane of every
/// tab and every workspace has index 0. One id therefore passes from surface
/// to surface with no pass in between where it is absent, and only its owner
/// tells the two surfaces apart.
#[derive(PartialEq, Eq)]
pub(crate) enum PoolOwner {
    Editor(WorkspaceId, EditorId),
    Terminal(WorkspaceId, TermId),
}

/// Flush the frame's APC decoration scene to the channel, when it changed.
///
/// A no-op until [`Stoat::stoatty`] confirms a listener. [`ApcScene::flush_to`]
/// writes nothing when the scene matches the previous flush, so steady-state
/// or widget-free frames push no batch at all. Runs at the frame seam after
/// the live frame is published, beside [`emit_smooth_scroll`].
pub(crate) fn emit_apc_scene(stoat: &mut Stoat) {
    let Some(apc_tx) = stoat.apc_tx.clone().filter(|_| stoat.stoatty) else {
        return;
    };

    let mut batch = Vec::new();
    let _ = stoat.apc_scene.flush_to(&mut batch);
    if !batch.is_empty() {
        let _ = apc_tx.send(batch);
    }
}

/// Push the active theme's colors to the hosting terminal as its defaults.
///
/// Normalized cells cover the grid, but the terminal resolves its own
/// defaults wherever no cell reaches, so without this the window gutter past
/// the grid keeps showing the host's theme. Sent once a listener is
/// confirmed and on every theme switch after that. A no-op until
/// [`Stoat::stoatty`] confirms one, and when the theme defines neither color
/// as RGB.
///
/// Paired with [`emit_reset_default_colors`], which gates the same way
/// so a session that never colored the host never uncolors it either.
pub(crate) fn emit_theme_default_colors(stoat: &Stoat) {
    let Some(apc_tx) = stoat.apc_tx.clone().filter(|_| stoat.stoatty) else {
        return;
    };

    let out = osc_default_colors(&stoat.theme);
    if !out.is_empty() {
        let _ = apc_tx.send(out);
    }
}

/// Hand the hosting terminal its own default colors back as the session
/// ends, so quitting does not leave the enclosing terminal recolored.
///
/// Sent once the run loop breaks. `apc_tx` is unbounded and the UI thread
/// only observes the channel closing after draining what is queued, so this
/// reaches stdout even though the process is on its way out.
///
/// A no-op until [`Stoat::stoatty`] confirms a listener, which is what keeps
/// it paired with [`emit_theme_default_colors`].
pub(crate) fn emit_reset_default_colors(stoat: &Stoat) {
    let Some(apc_tx) = stoat.apc_tx.clone().filter(|_| stoat.stoatty) else {
        return;
    };
    let _ = apc_tx.send(OSC_RESET_DEFAULT_COLORS.to_vec());
}

/// Tell the hosting terminal its config file changed on disk, so it re-reads
/// and re-applies it.
///
/// A no-op until [`Stoat::stoatty`] confirms a listener. Unlike the
/// frame-seam emitters this carries no state to reconcile. Each call reports
/// one save, and the terminal reads the file itself.
pub(crate) fn emit_config_reload(stoat: &Stoat) {
    let Some(apc_tx) = stoat.apc_tx.clone().filter(|_| stoat.stoatty) else {
        return;
    };

    let mut out = Vec::new();
    stoatty_protocol::command::encode_config_reload_into(&mut out);
    let _ = apc_tx.send(out);
}

/// Claim the platform zoom combo from the hosting terminal, or release it.
///
/// While claimed the terminal delivers each press here instead of stepping its
/// own font size, which is what lets the combo mean whatever the current
/// context calls for.
///
/// The claim asks for the press in band, as CSI-u down the pty. That is the one
/// route to a stoat running on the far end of a link, where the window socket
/// the terminal would otherwise use names a path on the wrong machine. A
/// terminal predating the mode ignores the request and forwards over the socket
/// as it always did, which is the same claim honored the older way.
///
/// [`Stoat::sync_zoom_claim`] decides when to call this and holds the reason.
///
/// A no-op until [`Stoat::stoatty`] confirms a listener, since under any
/// other terminal the combo never reaches stoat at all and that terminal
/// keeps its own font zoom.
pub(crate) fn emit_zoom_capture(stoat: &Stoat, on: bool) {
    let Some(apc_tx) = stoat.apc_tx.clone().filter(|_| stoat.stoatty) else {
        return;
    };

    let mut out = Vec::new();
    stoatty_protocol::command::encode_zoom_capture_into(&mut out, on, true);
    let _ = apc_tx.send(out);
}

/// Ask the hosting terminal to step its font size by `delta`, positive to
/// grow.
///
/// The way font zoom is reached now that the platform combo belongs to
/// stoat. Sending is unconditional here, so the caller decides whether a
/// terminal is listening and reports it otherwise.
pub(crate) fn emit_font_step(stoat: &Stoat, delta: i32) {
    let Some(apc_tx) = stoat.apc_tx.clone() else {
        return;
    };

    let mut out = Vec::new();
    stoatty_protocol::command::encode_font_step_into(&mut out, delta);
    let _ = apc_tx.send(out);
}

/// Open and close the aux windows detached panes render into.
///
/// Diffs the [`Stoat::aux_windows`] ledger against the active workspace's
/// detached panes. A newly detached pane emits a WindowOpen sized to its
/// window-relative area and titled by its buffer, and a ledger window whose
/// pane has reattached or closed emits a WindowClose. Runs at the frame seam
/// before [`emit_smooth_scroll`], so a window exists before its pool
/// content ships. An unchanged set sends nothing.
pub(crate) fn emit_windows(stoat: &mut Stoat) {
    let Some(apc_tx) = stoat.apc_tx.clone() else {
        return;
    };

    // No pane is detached and none was last frame either, so there is
    // nothing to open and nothing to close. That is the overwhelmingly
    // common shape, and the reason this checks before it builds anything.
    if !stoat.active_workspace().panes.has_windowed_panes() && stoat.aux_windows.is_empty() {
        return;
    }

    let (out, current) = {
        let ws = stoat.active_workspace();
        let mut out = Vec::new();
        let mut current: std::collections::BTreeMap<u32, (u16, u16)> =
            std::collections::BTreeMap::new();
        for (pane_id, window) in ws.panes.windowed_panes() {
            let pane = ws.panes.pane(pane_id);
            let size = (pane.area.width, pane.area.height);
            current.insert(window, size);
            if !stoat.aux_windows.contains_key(&window) {
                let title = match &pane.view {
                    View::Editor(editor_id) => ws
                        .editors
                        .get(*editor_id)
                        .and_then(|editor| ws.buffers.path_for(editor.buffer_id))
                        .and_then(|path| path.file_name())
                        .and_then(|name| name.to_str())
                        .map(String::from)
                        .unwrap_or_else(|| "stoat".to_string()),
                    _ => "stoat".to_string(),
                };
                stoatty_protocol::command::encode_window_open_into(
                    &mut out,
                    &stoatty_protocol::command::WindowOpenCommand {
                        window,
                        cols: size.0,
                        rows: size.1,
                        title,
                    },
                );
            }
        }
        for &window in stoat.aux_windows.keys() {
            if !current.contains_key(&window) {
                stoatty_protocol::command::encode_window_close_into(
                    &mut out,
                    &stoatty_protocol::command::WindowCloseCommand { window },
                );
            }
        }
        (out, current)
    };

    stoat.aux_windows = current;
    if !out.is_empty() {
        let _ = apc_tx.send(out);
    }
}

/// Ship each detached pane's window-bound surfaces to its aux window.
///
/// Every detached pane gets a one-row status pool. A non-editor pane also
/// gets a full-window content pool painted here, since only editors stream
/// through the async editor pool.
///
/// Both are single-page repaint surfaces, content-versioned so an unchanged
/// pane re-declares nothing. One [`crate::render::FrameCtx`] serves both
/// painters. It is built inline rather than by a `frame_ctx(stoat)` helper
/// because a whole-`Stoat` borrow would conflict with the `&mut ws.editors`
/// paint.
fn emit_window_content(stoat: &mut Stoat, out: &mut Vec<u8>) {
    let mode = stoat.focused_mode().to_string();
    let lsp_pending = crate::lsp::lsp_pending_label(stoat);
    // Resolved before the workspace borrow. The constructor reads the whole
    // &Stoat, and the dials are three separate fields on it.
    let preview_dials = crate::render::commits::PreviewDials::from_stoat(stoat);
    stoat.refresh_chrome();
    let ws = &mut stoat.workspaces[stoat.active_workspace];
    let windowed = ws.panes.windowed_panes();
    if windowed.is_empty() {
        return;
    }

    let workspace_name = ws.display_name().to_owned();
    let screen = crate::keymap_state::view_predicate(ws);
    let focus_target = ws.focus;
    let focus_id = ws.panes.focus();

    let diff_base_lead = ws.diff_base().and_then(crate::render::pane::diff_base_lead);
    let diff_sides = diff::diff_sides(ws.diff_base());
    let held_buffers = ws.held_buffer_labels();

    let frame = crate::render::FrameCtx {
        workspace_name: &workspace_name,
        workspace_root: &ws.git_root,
        mode: &mode,
        screen,
        theme: &stoat.theme,
        commits_split: stoat.commits_split,
        preview_dials,
        chrome: &stoat.chrome.as_ref().expect("refresh_chrome ran above").1,
        pending_count: stoat.pending_count,
        recording_register: stoat
            .macro_recording
            .as_ref()
            .map(|rec| rec.register.name()),
        held_buffers: &held_buffers,
        lsp_status_open: false,
        lsp_progress_entries: &[],
        spinner_phase: 0,
        lsp_servers: &[],
        diff_warm_busy: false,
        repo_change_counts: ws.repo_change_counts(),
        hunk_position: diff::repo_hunk_position(ws),
        diff_base: diff_base_lead.as_deref(),
        diff_sides: (&diff_sides.0, &diff_sides.1),
        lsp_pending,
        lsp_message: stoat
            .lsp_message
            .as_ref()
            .map(|(typ, message)| (*typ, message.as_str())),
        status_message: stoat.pending_message.as_deref(),
        goto_word_labels: None,
        mode_label: crate::render::pane::mode_segment(
            &mode,
            &stoat.theme,
            &stoat.settings.mode_badges,
        ),
        diagnostics: &stoat.diagnostics,
        search_query: None,
        search_smart_case: true,
        search_prompt: None,
        line_numbers: LineNumbers::Relative,
        wrap_mode: WrapMode::EditorWidth,
        wrap_column: 80,
        inactive_dim: 0.0,
        diff_dials: DiffDials::shipped(),
        images_capable: false,
        minimap_enabled: false,
        minimap_chrome: None,
        minimap_band: None,
        badge_cover: None,
        hover_cell: None,
        home: stoat.home.as_deref(),
        #[cfg(feature = "perf")]
        perf: None,
    };

    let split_count = ws.panes.split_panes().count();
    let show_badges = mode == "space_pane_display";

    for (windowed_index, (pane_id, window)) in windowed.into_iter().enumerate() {
        let (content, status, view, index, is_focused, area) = {
            let pane = ws.panes.pane(pane_id);
            let (content, status) = crate::render::layout::split_pane_status(pane.area);
            let is_focused = matches!(focus_target, FocusTarget::SplitPane) && pane_id == focus_id;
            (
                content,
                status,
                pane.view.clone(),
                pane.index,
                is_focused,
                pane.area,
            )
        };

        let region = PoolRegionCommand {
            pool: index,
            top: content.y,
            left: content.x,
            width: content.width,
            height: content.height,
            window,
            kind: PoolKind::Grid,
        };
        // A view whose render reads from a source that counts its own
        // changes answers "nothing moved" from the counter, and skipping on
        // that answer skips the render, the copy, and the serialize -- the
        // work that otherwise runs only to produce the hash deciding it.
        let input_version =
            window_content_version(&view, region, is_focused, stoat.theme_epoch, ws);
        let unchanged = input_version
            .is_some_and(|version| stoat.smooth_scroll.already_emitted(index, version));

        if !matches!(view, View::Editor(_)) && content.width > 0 && content.height > 0 && !unchanged
        {
            let mut buf = crate::smooth_scroll::page_buffer(area, &stoat.theme);
            {
                let mut scene = ApcScene::new();
                let mut undercurls = UndercurlBatch::default();
                crate::render::pane::render_pane(
                    ws.panes.pane(pane_id),
                    is_focused,
                    crate::render::PaneCtx {
                        editors: &mut ws.editors,
                        buffers: &ws.buffers,
                        runs: &ws.runs,
                        terms: &ws.terms,
                        term_pool_grids: &ws.term_pool_grids,
                        commit_lists: &mut ws.commit_lists,
                    },
                    frame,
                    &mut buf,
                    &mut scene,
                    &mut undercurls,
                    &mut None,
                );
            }

            // render_pane also paints the status row. The status pool below
            // owns it, so only the content rows are copied out and shipped.
            let mut content_buf = Buffer::empty(content);
            for y in content.top()..content.bottom() {
                for x in content.left()..content.right() {
                    content_buf[(x, y)] = buf[(x, y)].clone();
                }
            }

            let bytes = crate::render::serialize_buffer(&content_buf);
            // A view with no tracked inputs falls back to hashing what it
            // painted, which still suppresses the fill even though it could
            // not suppress the paint.
            let content_version = input_version.unwrap_or_else(|| {
                let mut hasher = DefaultHasher::new();
                stoat.theme_epoch.hash(&mut hasher);
                bytes.hash(&mut hasher);
                hasher.finish()
            });
            // A non-editor pane holds a single page, so only page 0 fills
            // and the rest of the buffered window stays empty.
            pool::emit_into(
                out,
                &mut stoat.smooth_scroll,
                region,
                0.0,
                content_version,
                false,
                |page| {
                    if page == 0 {
                        bytes.clone()
                    } else {
                        Vec::new()
                    }
                },
            );
        }

        if status.width == 0 || status.height == 0 {
            continue;
        }

        let badge = show_badges
            .then(|| split_count + windowed_index)
            .filter(|&position| position < 10)
            .map(|position| (position as u32 + 1) % 10);

        let row = Rect::new(0, 0, status.width, 1);
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::WINDOW_STATUS + index,
            top: status.y,
            left: status.x,
            width: status.width,
            height: 1,
            window,
            kind: PoolKind::Grid,
        };

        // The segments are the row's content, so hashing them answers
        // whether it moved without painting it. Assembling a handful of
        // short strings is the cheap half of what the row used to cost
        // every frame. Filling its cells and serializing them is the rest.
        let cells = crate::render::pane::pane_status_cells(
            &view,
            is_focused,
            row,
            frame,
            &mut ws.editors,
            &ws.buffers,
            &ws.terms,
            badge,
        );
        let content_version = {
            let mut hasher = DefaultHasher::new();
            stoat.theme_epoch.hash(&mut hasher);
            region.hash(&mut hasher);
            cells.base_style.hash(&mut hasher);
            cells.left.hash(&mut hasher);
            cells.right.hash(&mut hasher);
            hasher.finish()
        };
        if stoat
            .smooth_scroll
            .already_emitted(region.pool, content_version)
        {
            continue;
        }

        let mut buf = crate::smooth_scroll::page_buffer(row, &stoat.theme);
        crate::render::pane::paint_pane_status_cells(&cells, row, &mut buf);
        let bytes = crate::render::serialize_buffer(&buf);
        pool::emit_into(
            out,
            &mut stoat.smooth_scroll,
            region,
            0.0,
            content_version,
            false,
            |_| bytes.clone(),
        );
    }
}

/// Emit the stoatty smooth-scroll APC for every visible editor pane's current
/// scroll position, pushing one byte batch onto the APC channel.
///
/// A no-op until [`Stoat::stoatty`] confirms a listener. That gate matters
/// more here than on the other emitters. A fill frame carries its page as
/// raw ANSI between APC markers, so a terminal that drops the markers prints
/// the payload over whatever the screen was showing.
///
/// Each plain-editor split pane (a [`View::Editor`] that is not a review
/// view) gets its own pool, keyed by the pane's stable index, so split panes
/// glide independently and at once. A pane
/// that is no longer pooled -- closed, switched to another view, turned into a
/// review, or hidden behind a full-screen overlay (commits, rebase, reword,
/// conflict) -- is retired with `pool_drop`, so returning to it re-declares the
/// region and refills the page window.
///
/// A pool that took the cursor anchor is released with `pool_cursor_release` in
/// the first pass after its pane stops being the focused editor.
///
/// Runs at the frame seam after the live frame is published, so the pane
/// layout (and thus each editor rectangle) reflects the frame just drawn and
/// the APC bytes are written to stdout right after the grid frame.
pub(crate) fn emit_smooth_scroll(stoat: &mut Stoat) {
    let Some(apc_tx) = stoat.apc_tx.clone().filter(|_| stoat.stoatty) else {
        return;
    };
    stoat.refresh_chrome();

    // Every pool's rectangle moves again on the next event of a resize burst,
    // so the pools render no page for one until the burst's settle deadline
    // passes. The timer that deadline was armed with brings the frame that
    // fills them.
    let unsettled = stoat
        .pool_settle
        .as_ref()
        .is_some_and(|(at, _)| stoat.executor.now() < *at);
    stoat.smooth_scroll.set_geometry_unsettled(unsettled);

    // A full-screen overlay screen hides every editor, so nothing is pooled
    // this frame and any live pools are retired. The diff screen renders in
    // the real editor pool, so it is not an overlay.
    let overlay = matches!(
        crate::keymap_state::view_predicate(stoat.active_workspace()),
        Some("commits" | "rebase" | "reword" | "rebase_conflict")
    );
    let panes = if overlay {
        Vec::new()
    } else {
        // Detached editor panes ship through the same fill pipeline, their
        // regions bound to their aux windows. An overlay blanks them too.
        let mut panes = editor_pool_panes(stoat);
        panes.extend(windowed_editor_pool_panes(stoat));
        panes
    };

    // The file finder is a modal over normal mode (not a full-screen overlay
    // mode); its result list pools as a non-pane surface above the panes.
    let finder_list = (!overlay)
        .then_some(stoat.file_finder.as_ref())
        .flatten()
        .and_then(|finder| {
            crate::render::file_finder::file_finder_layout(
                stoat.size(),
                finder.content_size,
                modal_zoom_steps(&stoat.modal_zoom, ModalKind::FileFinder),
                modal_split_percent(&stoat.modal_split, ModalKind::FileFinder),
            )
        })
        .map(|layout| layout.list);

    // Every boxed modal previews through a scratch editor pane, and only one
    // modal is ever open, so one pool serves whichever is up. The pairing
    // excludes the modals whose preview is not an editor: the commit picker
    // pools its diff separately, and help and the run modal keep row offsets
    // rather than a buffer.
    let modal_preview = (!overlay)
        .then(|| {
            let rect = crate::mouse::boxed_modal_surfaces(stoat)?.preview?;
            let editor = crate::mouse::modal_preview_editor(stoat)?;
            (rect.width > 0 && rect.height > 0).then_some((rect, editor))
        })
        .flatten();

    // Code search asks for the whole area rather than measuring a match list it
    // would only ever outgrow, so its list rect comes from the finder layout at
    // an unbounded content size, exactly as its render asks for it.
    let code_search_list = (!overlay)
        .then_some(stoat.code_search.as_ref())
        .flatten()
        .and_then(|_| {
            crate::render::file_finder::file_finder_layout(
                stoat.size(),
                (u16::MAX, u16::MAX),
                modal_zoom_steps(&stoat.modal_zoom, ModalKind::CodeSearch),
                modal_split_percent(&stoat.modal_split, ModalKind::CodeSearch),
            )
        })
        .map(|layout| layout.list);

    // The symbol finder's box grows with its symbol count, so the layout needs
    // that count to land on the same rect the render paints into.
    let symbol_finder_list = (!overlay)
        .then_some(stoat.symbol_finder.as_ref())
        .flatten()
        .and_then(|finder| {
            crate::render::symbol_finder::symbol_finder_layout(
                stoat.size(),
                finder.content_rows,
                modal_zoom_steps(&stoat.modal_zoom, ModalKind::SymbolFinder),
                modal_split_percent(&stoat.modal_split, ModalKind::SymbolFinder),
            )
        })
        .map(|(_, _, list, _)| list);

    // The command palette is a modal over normal mode like the finder. Its
    // fixed list region pools as a non-pane surface. Command-filter mode
    // pools the command list.
    let palette_list = (!overlay)
        .then_some(stoat.command_palette.as_ref())
        .flatten()
        .filter(|p| p.command.is_none())
        .and_then(|p| {
            crate::render::command_palette::palette_filter_layout(
                stoat.size(),
                p.list_rows_hint(),
                modal_zoom_steps(&stoat.modal_zoom, ModalKind::Palette),
            )
        })
        .map(|layout| layout.list);

    // Argument mode (`:o `/`:cd `/`:b `) shows the inline picker in place of
    // the command list, and its result list pools into the same PALETTE id.
    // Filter and arg modes are mutually exclusive -- arg mode needs a parsed
    // command, filter mode needs none -- so one pool id serves both.
    let palette_arg_list = (!overlay)
        .then_some(stoat.command_palette.as_ref())
        .flatten()
        .filter(|p| p.arg_picker.is_some() && p.arg_source().is_some())
        .and_then(|p| {
            crate::render::command_palette::palette_arg_list_rect(
                stoat.size(),
                p.list_rows_hint(),
                modal_zoom_steps(&stoat.modal_zoom, ModalKind::Palette),
            )
        });

    // The commits screen renders into the focused pane. Its left list pools as
    // a non-pane surface, while the editor panes stay suppressed under it.
    let commits_screen =
        crate::keymap_state::view_predicate(stoat.active_workspace()) == Some("commits");
    let commits_region = commits_screen
        .then(|| {
            let ws = stoat.active_workspace();
            ws.focused_commits()?;
            let pane = ws.panes.pane(ws.panes.focus());
            crate::render::commits::commits_list_rect(pane.area, stoat.commits_split)
        })
        .flatten();

    // The commit picker's table and graph column scroll as one, so they
    // pool as one region. The header above and the diff below stay outside
    // it, since neither moves with the rows.
    let commit_picker_body = (!overlay)
        .then_some(stoat.commit_picker.as_ref())
        .flatten()
        .and_then(|picker| {
            crate::render::commit_picker::commit_picker_layout(
                stoat.size(),
                picker.graph_lanes,
                modal_zoom_steps(&stoat.modal_zoom, ModalKind::CommitPicker),
                modal_split_percent(&stoat.modal_split, ModalKind::CommitPicker),
            )
        })
        .map(|layout| layout.body());

    // The diff below the table scrolls on its own offset, so it pools
    // separately. It waits for the selected commit's session to build,
    // because a page with no diff would composite blank cells over the live
    // "loading diff..." message.
    let commit_picker_preview = (!overlay)
        .then_some(stoat.commit_picker.as_ref())
        .flatten()
        .filter(|picker| {
            picker
                .selected_commit()
                .is_some_and(|c| matches!(picker.preview_sessions.get(&c.sha), Preview::Built(_)))
        })
        .and_then(|picker| {
            crate::render::commit_picker::commit_picker_layout(
                stoat.size(),
                picker.graph_lanes,
                modal_zoom_steps(&stoat.modal_zoom, ModalKind::CommitPicker),
                modal_split_percent(&stoat.modal_split, ModalKind::CommitPicker),
            )
        })
        .and_then(|layout| layout.preview);

    // The completion popup is cursor-anchored: its inner list region pools
    // and moves with the cursor each frame (emit_into re-emits the region on
    // a move). The layout reads the focused editor, so it borrows stoat.
    let completion = (!overlay)
        .then(|| crate::render::completion::frame_layout(stoat))
        .and_then(|memo| memo.layout);

    // The help view is a fixed centered modal over the editor like the
    // finder; its list and detail panes pool as two non-pane surfaces.
    let help_layout = (!overlay)
        .then_some(stoat.help.as_ref())
        .flatten()
        .and_then(|help| {
            crate::render::help::help_layout(
                stoat.size(),
                crate::render::help::help_content_rows(help),
                modal_zoom_steps(&stoat.modal_zoom, ModalKind::Help),
            )
        });

    // The hover popup is cursor-anchored like the completion popup. Its
    // interior body region pools so Ctrl-u/Ctrl-d and the wheel ease. The
    // layout reads the focused editor, so it borrows stoat.
    // A live hover selection is painted by the live frame's highlight, so
    // the pooled body defers, since its glide path carries no selection.
    // Skipping the layout retires the HOVER pool via drop_absent below.
    let hover_selected = stoat
        .pending_hover
        .as_ref()
        .and_then(|p| p.selection.as_ref())
        .is_some();
    // Mid-glide the popup's own layout is frozen at the rect the last live frame
    // stamped. Re-laying it against the jumped-ahead scroll target is what parts
    // the body from the frame. The frame rides the anchor while the body jumps
    // to where the target lands.
    let hover_frozen = stoat
        .pending_hover
        .as_ref()
        .filter(|popup| popup.area.width > 0 && popup.area.height > 0)
        .filter(|popup| {
            stoat
                .active_workspace()
                .editors
                .get(popup.editor_id)
                .is_some_and(|editor| editor.scroll_glide != ScrollGlide::None)
        })
        .map(|popup| (popup.area, popup.inner));
    let hover_layout = (!overlay && !hover_selected)
        .then(|| hover_frozen.or_else(|| crate::render::hover::hover_popup_layout(stoat)))
        .flatten();

    let inactive_dim = stoat
        .settings
        .ui_inactive_dim
        .unwrap_or(0.25)
        .clamp(0.0, 1.0) as f32;

    // A terminal or agent pane draws at the terminal font size as a terminal
    // pool, which a terminal before that protocol does not know. An overlay
    // screen hides it with the editors.
    let terminal_panes = match overlay || stoat.stoatty_protocol < TERMINAL_POOL_PROTOCOL {
        true => Vec::new(),
        false => terminal_pool_panes(stoat, inactive_dim),
    };

    let mut out = Vec::new();
    drop_reowned_pools(stoat, &mut out, &panes, &terminal_panes);

    let mut active: Vec<u32> = panes.iter().map(|(pool, _, _)| *pool).collect();
    active.extend(terminal_panes.iter().map(|pane| pane.region.pool));
    // Each detached pane also keeps a one-row status pool alive.
    for (pool, _, region) in &panes {
        if region.window != 0 {
            active.push(crate::smooth_scroll::non_pane_pool::WINDOW_STATUS + pool);
        }
    }
    // Detached non-editor panes repaint through a full-window content pool
    // plus a status row. The editor pool loop above declares neither.
    {
        let ws = stoat.active_workspace();
        for (pane_id, _) in ws.panes.windowed_panes() {
            let pane = ws.panes.pane(pane_id);
            if matches!(pane.view, View::Editor(_)) {
                continue;
            }
            active.push(pane.index);
            active.push(crate::smooth_scroll::non_pane_pool::WINDOW_STATUS + pane.index);
        }
    }
    if finder_list.is_some() {
        active.push(crate::smooth_scroll::non_pane_pool::FINDER);
    }
    if palette_list.is_some() || palette_arg_list.is_some() {
        active.push(crate::smooth_scroll::non_pane_pool::PALETTE);
    }
    if code_search_list.is_some() {
        active.push(crate::smooth_scroll::non_pane_pool::CODE_SEARCH_LIST);
    }
    if symbol_finder_list.is_some() {
        active.push(crate::smooth_scroll::non_pane_pool::SYMBOL_FINDER_LIST);
    }
    if modal_preview.is_some() {
        active.push(crate::smooth_scroll::non_pane_pool::MODAL_PREVIEW);
    }
    if commits_region.is_some() {
        active.push(crate::smooth_scroll::non_pane_pool::COMMITS);
    }
    if commit_picker_body.is_some() {
        active.push(crate::smooth_scroll::non_pane_pool::COMMIT_PICKER_LIST);
    }
    if commit_picker_preview.is_some() {
        active.push(crate::smooth_scroll::non_pane_pool::COMMIT_PICKER_PREVIEW);
    }
    if completion.is_some() {
        active.push(crate::smooth_scroll::non_pane_pool::COMPLETION);
    }
    if help_layout.is_some() {
        active.push(crate::smooth_scroll::non_pane_pool::HELP_LIST);
        active.push(crate::smooth_scroll::non_pane_pool::HELP_DETAIL);
    }
    if hover_layout.is_some() {
        active.push(crate::smooth_scroll::non_pane_pool::HOVER);
    }
    stoat.smooth_scroll.drop_absent(&mut out, &active);

    // Editor and review pages render off the run loop. The loop collects a
    // snapshot -- plus the cloned view state and theme for review -- and the
    // newly-entered page indices per pane, then spawns the renders after the
    // APC batch ships, so region and scroll always reach the terminal before
    // any fill.
    struct ConflictFillParts {
        state: Arc<crate::conflict_session::ConflictViewState>,
        theme: Arc<crate::theme::Theme>,
    }
    enum PoolFill {
        Editor {
            snapshot: DisplaySnapshot,
            pages: Vec<u64>,
            /// Buffered pages whose gutter runs changed while their cells did
            /// not, sent after the whole pages as decorations-only fills.
            redecorate: Vec<u64>,
            pool: u32,
            width: u16,
            height: u16,
            gutter: crate::smooth_scroll::PageGutter,
            diff_view: bool,
            dim: f32,
            dials: DiffDials,
            /// The walkthrough spotlight the live grid paints on this editor.
            spotlight: Option<Spotlight>,
        },
        Conflict {
            snapshot: DisplaySnapshot,
            parts: Box<ConflictFillParts>,
            pages: Vec<u64>,
            pool: u32,
            width: u16,
            height: u16,
        },
    }
    let mut async_jobs: Vec<PoolFill> = Vec::new();
    // Found before the closures below borrow `stoat` for the rest of the emit,
    // because the search measures the focused editor through a mutable borrow.
    let spotlight = walkthrough::spotlight_of(stoat);
    let syntax_highlight = stoat.syntax_highlight;
    // What each editor actually paints with, which is what a page's hash has to
    // carry. The diff toggle only subtracts, and only from a diff view.
    let editor_syntax = |diff_view: bool| syntax_highlight && (stoat.diff_syntax || !diff_view);
    let line_numbers = stoat
        .settings
        .editor_line_numbers
        .unwrap_or(LineNumbers::Relative);
    let diff_dials = DiffDials::from_stoat(stoat);
    // Relative numbering follows the same pane the live render calls focused:
    // the focused split editor outside insert mode. Resolved before the ws
    // borrow so the per-pane loop can gate on it.
    let focused_editor = stoat.focused_editor_ids().map(|(id, _)| id);
    let focused_pool = panes
        .iter()
        .find(|(_, id, region)| region.window == 0 && focused_editor == Some(*id))
        .map(|(_, _, region)| region.pool);
    let focused_insert = stoat.focused_mode() == "insert";
    let single_minimap = stoat.single_minimap_rect.is_some();
    // The focused detached pane's pool cursor, collected during the pane loop
    // and emitted after it, past the workspace borrow.
    let mut detached_cursor: Option<(u32, u64, u16)> = None;
    // The pool that takes the cursor anchor in this pass, recorded after the
    // loop, past the workspace borrow.
    let mut anchored_pool: Option<u32> = None;
    let theme_epoch = stoat.theme_epoch;
    let ws = &mut stoat.workspaces[stoat.active_workspace];
    let theme = &stoat.theme;
    let fallback_style = theme.get(crate::theme::scope::UI_TEXT);
    // Resolved once for the theme rather than per pane per frame.
    let base_rich = stoat
        .chrome
        .as_ref()
        .and_then(|(_, chrome)| chrome.rich_gutter.clone());
    let decorate_pages = stoat.stoatty_protocol >= FILL_DECORATIONS_PROTOCOL;
    for (_, editor_id, region) in &panes {
        let region = *region;
        let Some(editor) = ws.editors.get_mut(*editor_id) else {
            continue;
        };
        // A pooled page for an unfocused pane carries the same dim as its
        // live grid, so a wheel glide over it never flashes bright content.
        let dim = if focused_editor == Some(*editor_id) {
            0.0
        } else {
            inactive_dim
        };
        // The walkthrough spotlight rides the pages of the editor the live grid
        // paints it on, so a glide composites the lit annotation and the dim.
        let page_spotlight = spotlight
            .as_ref()
            .filter(|(lit, _)| lit == editor_id)
            .map(|(_, spotlight)| spotlight);
        // scroll_row is the source of truth for the pool page. The wheel
        // glide refines it sub-row through scroll_offset, but cursor-follow
        // and jumps move scroll_row without the fraction, so trust the offset
        // only while it still floors to scroll_row. A page glide is the
        // exception. scroll_row jumped to the target and the offset lags
        // behind easing up to it, so trust the fraction throughout the glide
        // and let the pool ease from the lagging offset to the target.
        let scroll_offset = if editor.scroll_glide != ScrollGlide::None
            || editor.scroll_offset.floor() as u32 == editor.scroll_row
        {
            editor.scroll_offset
        } else {
            editor.scroll_row as f32
        };
        // Review rows regenerate on accept/reject and their gutter glyphs
        // change on stage/unstage, so the session version is the pool's
        // content version. Plain editors stay stable while scrolling, save
        // for the syntax-highlight toggle (recolors every row), a diagnostics
        // change (restyles the gutter), and a gutter-width change (reflows the
        // inset), so a buffered page must refill when any of those move. The
        // emit below holds that refill while the pool's scroll target rests,
        // deferring it until the target next moves.
        //
        // Relative numbers reference the cursor's buffer line. The wheel
        // glide's cursor-follow drags that line every row, so a wheel glide
        // holds the baked line until the settle, which keeps the page versions
        // stable instead of refreshing the window per dragged row. The settle
        // emit recomputes the live line and refreshes the window once, as whole
        // pages or as gutter runs. A page glide lands the cursor before it
        // arms, so its pages carry the landed line and its settle has nothing
        // to refresh.
        let current_line = if editor.scroll_glide == ScrollGlide::Wheel {
            editor.pool_current_line
        } else {
            let line = (line_numbers == LineNumbers::Relative
                && !focused_insert
                && focused_editor == Some(*editor_id))
            .then(|| crate::render::editor::editor_cursor_position(editor).map(|(line, _)| line))
            .flatten();
            editor.pool_current_line = line;
            line
        };

        // The rich gutter lays its numbers over the page as runs, so a
        // terminal that reads decorations-only fills takes a cursor line change
        // as new runs over cells it keeps. The fallback gutter paints numbers
        // into cells, and diff and conflict pages paint gutters of their own, so
        // those keep the line in the cell version.
        let split_gutter = decorate_pages
            && base_rich.is_some()
            && !editor.diff_view
            && editor.conflict_view.is_none();
        let decoration_version = match split_gutter {
            true => {
                let mut hasher = DefaultHasher::new();
                current_line.hash(&mut hasher);
                hasher.finish()
            },
            false => 0,
        };

        // Version-cached, so the extra query is cheap. The pair carries every
        // edit and every layer reflow into the content hash below.
        let buffer_version = editor.display_map.buffer_snapshot().version();
        let paint_version = editor.display_map.snapshot().paint_version();
        let content_version = editor_page_content_version(
            editor_syntax(editor.diff_view),
            editor.gutter_width,
            editor.display_map.wrap_width(),
            current_line.filter(|_| !split_gutter),
            editor
                .gutter_severity_cache
                .as_ref()
                .map_or(0, |cache| cache.version),
            editor.diff_view,
            editor.display_map.diff_version(),
            dim,
            diff_dials,
            buffer_version,
            paint_version,
            theme_epoch,
            page_spotlight,
        );
        // The spotlight recolors only the rows it lights, so where it falls is
        // each page's own version. A diff-view page draws no spotlight.
        let lit = page_spotlight
            .filter(|_| !editor.diff_view)
            .map(|spotlight| {
                let snapshot = editor.display_map.snapshot();
                (
                    spotlight.color,
                    crate::smooth_scroll::spotlight_display_span(&snapshot, spotlight),
                )
            });
        let Refill {
            entered,
            redecorated,
        } = pool::emit_pages_into(
            &mut out,
            &mut stoat.smooth_scroll,
            region,
            scroll_offset,
            PageVersions {
                cells: content_version,
                decorations: decoration_version,
            },
            |index| spotlight_page_version(lit, index, region.height),
            // Editor panes refill constantly at rest for the cursor line,
            // focus dim, and diagnostics, so they hold the window until the
            // glide starts.
            true,
            // Editor pages fill asynchronously below, so the synchronous
            // render emits nothing here.
            |_| Vec::new(),
        );

        // Per-pane mode feeds each pane's own strip, keyed by its pool. Single
        // mode feeds one shared strip (u32::MAX) for the focused pane only.
        // Aux windows draw no minimap, so a windowed region feeds no strip.
        let view_strip = if region.window != 0 {
            None
        } else if single_minimap {
            (focused_editor == Some(*editor_id)).then_some(crate::render::SINGLE_MINIMAP_STRIP_ID)
        } else {
            editor.minimap_rect.is_some().then_some(region.pool)
        };
        if let Some(strip_id) = view_strip {
            // The strip is one row per buffer line while the editor scrolls in
            // display rows, so the window converts before it ships. The
            // conversion is deferred into the closure because an idle frame
            // has nothing to convert that the last one did not.
            let snapshot = editor.display_map.snapshot();
            let rows = region.height as u32;
            let inputs = MinimapWindowInputs::new(
                scroll_offset,
                display_map_stamp(buffer_version, paint_version),
                rows,
            );
            stoat
                .smooth_scroll
                .emit_minimap_view(&mut out, strip_id, region.pool, inputs, || {
                    minimap_view_window(&snapshot, scroll_offset, rows)
                });
        }

        // While the focused pane glides, ship the cursor's content anchor so
        // the terminal draws it riding the eased offset. A None cell (cursor
        // off-view at the last paint) skips the emit, so the terminal falls
        // back to its plain ease. A detached pane's cursor is handled just
        // below instead, so it is excluded here.
        //
        // An unaligned rest ships it too. The terminal holds its composite at
        // that fraction rather than handing the region back, so the drawn
        // cursor has to ride the shifted content after the glide ends.
        if region.window == 0
            && focused_editor == Some(*editor_id)
            && (editor.scroll_glide != ScrollGlide::None || editor.scroll_frac != 0.0)
            && let Some((col, _)) = editor.cursor_screen_cell
        {
            let row = view::cursor_display_row(editor) as u64;
            stoatty_protocol::command::encode_pool_cursor_into(
                &mut out,
                &stoatty_protocol::command::PoolCursorCommand {
                    pool: region.pool,
                    row,
                    col,
                },
            );
            anchored_pool = Some(region.pool);
        }

        // A focused detached pane draws its cursor from its window pool. No
        // live paint records its screen cell, so derive the display cell
        // directly. The actual emit is change-gated after the loop.
        if region.window != 0 && focused_editor == Some(*editor_id) {
            let (row, column) = view::cursor_display_cell(editor);
            let col = region.left + editor.gutter_width + column as u16;
            detached_cursor = Some((region.pool, row as u64, col));
        }

        if !entered.is_empty() || !redecorated.is_empty() {
            let snapshot = editor.display_map.snapshot();
            if let Some(state) = editor.conflict_view.as_ref() {
                // The conflict pane is a View::Editor, so without its own
                // arm it would fill through the plain-editor page and paint
                // the bare center buffer, dropping the flanking columns.
                async_jobs.push(PoolFill::Conflict {
                    snapshot,
                    parts: Box::new(ConflictFillParts {
                        state: state.clone(),
                        theme: theme.clone(),
                    }),
                    pages: entered,
                    pool: region.pool,
                    width: region.width,
                    height: region.height,
                });
            } else {
                let severity = editor
                    .gutter_severity_cache
                    .as_ref()
                    .map(|cache| cache.map.clone())
                    .unwrap_or_default();
                let rich = base_rich.clone().map(|r| r.dim(dim));
                async_jobs.push(PoolFill::Editor {
                    snapshot,
                    pages: entered,
                    redecorate: redecorated,
                    pool: region.pool,
                    width: region.width,
                    height: region.height,
                    gutter: crate::smooth_scroll::PageGutter::new(
                        line_numbers != LineNumbers::Off,
                        severity,
                        theme.clone(),
                        rich,
                        current_line,
                    ),
                    diff_view: editor.diff_view,
                    dials: diff_dials,
                    dim,
                    spotlight: page_spotlight.cloned(),
                });
            }
        }
    }

    // Ship the focused detached pane's cursor when it moved. An idle frame,
    // or focus leaving every detached pane, ships nothing.
    if detached_cursor != stoat.aux_cursor {
        if let Some((pool, row, col)) = detached_cursor {
            stoatty_protocol::command::encode_pool_cursor_into(
                &mut out,
                &stoatty_protocol::command::PoolCursorCommand { pool, row, col },
            );
        }
        stoat.aux_cursor = detached_cursor;
    }

    // An anchor outlives its glide in the terminal, so the pool keeps the cursor
    // on a later glide unless it is released when its pane loses focus. A
    // focused pane keeps its anchor through its own settle, since the terminal's
    // ease continues for some frames after the editor's ends. A pass with an
    // overlay screen has no panes, so it releases a held pool that the same pass
    // drops, which the terminal ignores.
    if let Some(pool) = stoat.pool_cursor_holder
        && focused_pool != Some(pool)
    {
        stoatty_protocol::command::encode_pool_cursor_release_into(&mut out, pool);
        stoat.pool_cursor_holder = None;
    }
    if anchored_pool.is_some() {
        stoat.pool_cursor_holder = anchored_pool;
    }

    // A terminal page is the emulator's screen, which is already at hand, so it
    // fills here rather than off the run loop as an editor page does.
    {
        let ws = &stoat.workspaces[stoat.active_workspace];
        for pane in &terminal_panes {
            let Some(term) = ws.terms.get(pane.term_id) else {
                continue;
            };
            emit_terminal_pool(
                &mut out,
                &mut stoat.smooth_scroll,
                term,
                ws.term_pool_grids.get(&pane.term_id).copied(),
                pane,
                theme,
                theme_epoch,
            );
        }
    }

    if let (Some(list), Some(finder)) = (finder_list, stoat.file_finder.as_ref()) {
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::FINDER,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
            window: 0,
            kind: PoolKind::Grid,
        };
        let core = finder.active_core_ref();
        let scroll_row = core
            .picklist
            .selected
            .saturating_sub(list.height.saturating_sub(1) as usize) as u32;
        // The visible rows are the active picker's filtered indices, and in
        // browse mode the typed directory re-roots them, so both feed the
        // pool's content version: a re-filter or a re-root refills it.
        let content_version = {
            let mut hasher = DefaultHasher::new();
            stoat.theme_epoch.hash(&mut hasher);
            finder
                .browse
                .as_ref()
                .map(|browse| browse.typed_dir.as_str())
                .unwrap_or_default()
                .hash(&mut hasher);
            core.picklist.filter_generation.hash(&mut hasher);
            hasher.finish()
        };
        let home = stoat.home.as_deref();
        pool::emit_into(
            &mut out,
            &mut stoat.smooth_scroll,
            region,
            scroll_row as f32,
            content_version,
            true,
            |page| {
                crate::smooth_scroll::render_finder_page(
                    finder,
                    home,
                    page,
                    theme,
                    region.width,
                    region.height,
                )
            },
        );
    }

    // The preview pane is an editor, so it pools exactly as a split pane does:
    // the same eased scroll target, the same content version, and pages filled
    // asynchronously through the editor fill path below.
    if let Some((rect, editor_id)) = modal_preview {
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::MODAL_PREVIEW,
            top: rect.y,
            left: rect.x,
            width: rect.width,
            height: rect.height,
            window: 0,
            kind: PoolKind::Grid,
        };
        let active_idx = stoat.active_workspace;
        if let Some(editor) = stoat.workspaces[active_idx].editors.get_mut(editor_id) {
            // Same rule the pane loop follows: the sub-row offset is the truth
            // while it still floors to scroll_row, and throughout a glide where
            // scroll_row has already jumped to the target.
            let scroll_offset = if editor.scroll_glide != ScrollGlide::None
                || editor.scroll_offset.floor() as u32 == editor.scroll_row
            {
                editor.scroll_offset
            } else {
                editor.scroll_row as f32
            };
            let content_version = editor_page_content_version(
                editor_syntax(editor.diff_view),
                editor.gutter_width,
                editor.display_map.wrap_width(),
                None,
                editor
                    .gutter_severity_cache
                    .as_ref()
                    .map_or(0, |cache| cache.version),
                editor.diff_view,
                editor.display_map.diff_version(),
                0.0,
                // A modal preview is not the diff view the knobs turn, so it
                // paints at the shipped dials.
                DiffDials::shipped(),
                editor.display_map.buffer_snapshot().version(),
                editor.display_map.snapshot().paint_version(),
                theme_epoch,
                None,
            );
            let snapshot = editor.display_map.snapshot();
            let severity = editor
                .gutter_severity_cache
                .as_ref()
                .map(|cache| cache.map.clone())
                .unwrap_or_default();
            let diff_view = editor.diff_view;

            let entered = pool::emit_into(
                &mut out,
                &mut stoat.smooth_scroll,
                region,
                scroll_offset,
                content_version,
                // A preview reloads on every selection step, so it holds the
                // window until the glide starts, as an editor pane does.
                true,
                |_| Vec::new(),
            );
            if !entered.is_empty() {
                async_jobs.push(PoolFill::Editor {
                    snapshot,
                    pages: entered,
                    redecorate: Vec::new(),
                    pool: region.pool,
                    width: region.width,
                    height: region.height,
                    gutter: crate::smooth_scroll::PageGutter::new(
                        line_numbers != LineNumbers::Off,
                        severity,
                        theme.clone(),
                        base_rich.clone(),
                        None,
                    ),
                    diff_view,
                    // A modal preview is not the diff view the knobs turn, so
                    // it paints at the shipped dials.
                    dials: DiffDials::shipped(),
                    dim: 0.0,
                    spotlight: None,
                });
            }
        }
    }

    if let (Some(list), Some(finder)) = (code_search_list, stoat.code_search.as_ref()) {
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::CODE_SEARCH_LIST,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
            window: 0,
            kind: PoolKind::Grid,
        };
        let scroll_row =
            crate::render::picker::window_start(finder.selected, list.height.max(1) as usize)
                as u32;
        // Matches stream in a batch at a time, so the count moves while a scan
        // runs and the pool refills behind it. The query covers a re-scan that
        // lands on the same count, and the invalid-pattern flag covers the
        // placeholder row, which is not a match at all.
        let content_version = {
            let mut hasher = DefaultHasher::new();
            stoat.theme_epoch.hash(&mut hasher);
            finder.last_query.hash(&mut hasher);
            finder.matches.len().hash(&mut hasher);
            finder.invalid_pattern.hash(&mut hasher);
            hasher.finish()
        };
        let git_root = stoat.active_workspace().git_root.clone();
        pool::emit_into(
            &mut out,
            &mut stoat.smooth_scroll,
            region,
            scroll_row as f32,
            content_version,
            true,
            |page| {
                crate::smooth_scroll::render_code_search_page(
                    finder,
                    &git_root,
                    page,
                    theme,
                    region.width,
                    region.height,
                )
            },
        );
    }

    if let (Some(list), Some(finder)) = (symbol_finder_list, stoat.symbol_finder.as_ref()) {
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::SYMBOL_FINDER_LIST,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
            window: 0,
            kind: PoolKind::Grid,
        };
        let scroll_row =
            crate::render::picker::window_start(finder.selected, list.height.max(1) as usize)
                as u32;
        // The ranking is a pure function of the query and the entry list, and
        // the finder already keys its skip-rerank check on exactly that pair,
        // so the same key versions the rows the pool paints.
        let content_version = {
            let mut hasher = DefaultHasher::new();
            stoat.theme_epoch.hash(&mut hasher);
            finder.last_filter_key.hash(&mut hasher);
            hasher.finish()
        };
        let git_root = stoat.active_workspace().git_root.clone();
        pool::emit_into(
            &mut out,
            &mut stoat.smooth_scroll,
            region,
            scroll_row as f32,
            content_version,
            true,
            |page| {
                crate::smooth_scroll::render_symbol_finder_page(
                    finder,
                    &git_root,
                    page,
                    theme,
                    region.width,
                    region.height,
                )
            },
        );
    }
    if let (Some(list), Some(palette)) = (palette_list, stoat.command_palette.as_ref()) {
        let filtered = &palette.filtered;
        let match_indices = &palette.match_indices;
        let selected = &palette.selected;
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::PALETTE,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
            window: 0,
            kind: PoolKind::Grid,
        };
        let scroll_row = selected.saturating_sub(list.height.saturating_sub(1) as usize) as u32;
        // The visible row set is the filtered entries, so a hash of their
        // names is the pool's content version and a re-filter refills it. The
        // leading discriminant keeps a filter-mode list from aliasing an
        // arg-mode list that shares this pool id and matches region and scroll.
        let content_version = {
            let mut hasher = DefaultHasher::new();
            stoat.theme_epoch.hash(&mut hasher);
            0u8.hash(&mut hasher);
            palette.generation.hash(&mut hasher);
            hasher.finish()
        };
        pool::emit_into(
            &mut out,
            &mut stoat.smooth_scroll,
            region,
            scroll_row as f32,
            content_version,
            true,
            |page| {
                crate::smooth_scroll::render_palette_page(
                    filtered,
                    match_indices,
                    *selected,
                    page,
                    theme,
                    region.width,
                    region.height,
                )
            },
        );
    }

    if let (Some(list), Some(picker)) = (
        palette_arg_list,
        stoat
            .command_palette
            .as_ref()
            .and_then(|palette| palette.arg_picker.as_ref()),
    ) {
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::PALETTE,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
            window: 0,
            kind: PoolKind::Grid,
        };
        let core = picker.active_core_ref();
        let scroll_row = core
            .picklist
            .selected
            .saturating_sub(list.height.saturating_sub(1) as usize) as u32;
        // The visible rows are the active picker's filtered paths, so their
        // hash is the pool's content version and a re-filter refills it. The
        // leading discriminant keeps this arg-mode list from aliasing a
        // filter-mode list that shares this pool id, and the browse typed
        // directory folds re-roots in so two same-shaped filtered sets from
        // different roots cannot alias.
        let content_version = {
            let mut hasher = DefaultHasher::new();
            stoat.theme_epoch.hash(&mut hasher);
            1u8.hash(&mut hasher);
            picker
                .browse
                .as_ref()
                .map(|browse| browse.typed_dir.as_str())
                .unwrap_or_default()
                .hash(&mut hasher);
            core.picklist.filter_generation.hash(&mut hasher);
            hasher.finish()
        };
        let home = stoat.home.as_deref();
        pool::emit_into(
            &mut out,
            &mut stoat.smooth_scroll,
            region,
            scroll_row as f32,
            content_version,
            true,
            |page| {
                crate::smooth_scroll::render_arg_page(
                    picker,
                    home,
                    page,
                    theme,
                    region.width,
                    region.height,
                )
            },
        );
    }

    if let (Some(list), Some(state)) = (
        commits_region,
        stoat.workspaces[stoat.active_workspace].focused_commits(),
    ) {
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::COMMITS,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
            window: 0,
            kind: PoolKind::Grid,
        };
        let scroll_row = state.scroll_top as u32;
        // Commits stream in lazily, so the length plus the load/end flags
        // form the content version; new commits refill the pages.
        let content_version = (state.commits.len() as u64) << 2
            | ((state.pending_load.is_some() as u64) << 1)
            | (state.reached_end as u64);
        pool::emit_into(
            &mut out,
            &mut stoat.smooth_scroll,
            region,
            scroll_row as f32,
            content_version,
            false,
            |page| {
                crate::smooth_scroll::render_commits_page(
                    state,
                    page,
                    theme,
                    region.width,
                    region.height,
                )
            },
        );
    }

    if let (Some(body), Some(picker)) = (commit_picker_body, stoat.commit_picker.as_ref()) {
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::COMMIT_PICKER_LIST,
            top: body.y,
            left: body.x,
            width: body.width,
            height: body.height,
            window: 0,
            kind: PoolKind::Grid,
        };
        let lanes = picker.graph_lanes;
        let scroll_row =
            crate::render::picker::window_start(picker.selected, body.height as usize) as u32;
        // A refilter rewrites every row, the column scope recolors them, and
        // the selection moves the highlight, so all three refill the pages.
        let content_version = {
            let mut hasher = DefaultHasher::new();
            stoat.theme_epoch.hash(&mut hasher);
            picker.filter_generation.hash(&mut hasher);
            picker.filter_column.map(|c| c as usize).hash(&mut hasher);
            picker.selected.hash(&mut hasher);
            lanes.hash(&mut hasher);
            hasher.finish()
        };
        pool::emit_into(
            &mut out,
            &mut stoat.smooth_scroll,
            region,
            scroll_row as f32,
            content_version,
            false,
            |page| {
                crate::smooth_scroll::render_commit_picker_list_page(
                    picker,
                    lanes,
                    page,
                    theme,
                    region.width,
                    region.height,
                )
            },
        );
    }

    if let (Some(rect), Some(picker)) = (commit_picker_preview, stoat.commit_picker.as_ref())
        && let Some(session) =
            picker
                .selected_commit()
                .and_then(|c| match picker.preview_sessions.get(&c.sha) {
                    Preview::Built(document) => Some(Arc::clone(document)),
                    Preview::Empty | Preview::Unbuilt => None,
                })
    {
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::COMMIT_PICKER_PREVIEW,
            top: rect.y,
            left: rect.x,
            width: rect.width,
            height: rect.height,
            window: 0,
            kind: PoolKind::Grid,
        };
        // Clamped through the same helper the renderer uses, so the pool
        // scrolls to the row the diff actually lands on.
        let scroll_row = crate::render::commit_picker::clamped_preview_scroll(
            picker.preview_scroll,
            &session,
            rect.height,
        );
        // A different commit means a different diff, and a rebuilt session
        // changes its length, so both refill the pages. A dial step repaints
        // every row it reaches, so every dial rides the hash too.
        let content_version = {
            let mut hasher = DefaultHasher::new();
            stoat.theme_epoch.hash(&mut hasher);
            picker
                .selected_commit()
                .map(|c| c.sha.as_str())
                .hash(&mut hasher);
            crate::render::commits::preview_row_count(&session).hash(&mut hasher);
            stoat.diff_soften.hash(&mut hasher);
            stoat.diff_tint.hash(&mut hasher);
            stoat.diff_syntax.hash(&mut hasher);
            stoat.diff_bold.hash(&mut hasher);
            stoat.diff_underline.hash(&mut hasher);
            hasher.finish()
        };
        let dials = crate::render::commits::PreviewDials::from_stoat(stoat);
        pool::emit_into(
            &mut out,
            &mut stoat.smooth_scroll,
            region,
            scroll_row as f32,
            content_version,
            false,
            |page| {
                crate::smooth_scroll::render_commit_picker_preview_page(
                    &session,
                    page,
                    theme,
                    dials,
                    region.width,
                    region.height,
                )
            },
        );
    }

    if let Some((prefix, layout)) = completion
        && let Some(popup) = stoat.pending_completion.as_ref()
    {
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::COMPLETION,
            top: layout.inner.y,
            left: layout.inner.x,
            width: layout.inner.width,
            height: layout.inner.height,
            window: 0,
            kind: PoolKind::Grid,
        };
        let scroll_row = layout.viewport_top as u32;
        // The item list is replaced wholesale on a re-query, which bumps
        // completion_generation, so that counter is the pool's content
        // version. A re-query refills without hashing every label each emit.
        let content_version = stoat.completion_generation;
        pool::emit_into(
            &mut out,
            &mut stoat.smooth_scroll,
            region,
            scroll_row as f32,
            content_version,
            true,
            |page| {
                crate::smooth_scroll::render_completion_page(
                    popup,
                    popup.selected_idx,
                    &prefix,
                    page,
                    theme,
                    region.width,
                    region.height,
                )
            },
        );
    }

    if let (Some(layout), Some(help)) = (help_layout, stoat.help.as_ref()) {
        let list = layout.list;
        let list_region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::HELP_LIST,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
            window: 0,
            kind: PoolKind::Grid,
        };
        let list_scroll =
            help.selected()
                .saturating_sub(list.height.saturating_sub(1) as usize) as u32;
        // The filtered entry set changes on every search refilter, so its
        // hash is the list pool's content version.
        let list_version = {
            let mut hasher = DefaultHasher::new();
            help.generation.hash(&mut hasher);
            hasher.finish()
        };
        pool::emit_into(
            &mut out,
            &mut stoat.smooth_scroll,
            list_region,
            list_scroll as f32,
            list_version,
            false,
            |page| {
                crate::smooth_scroll::render_help_list_page(
                    help,
                    page,
                    theme,
                    list.width,
                    list.height,
                )
            },
        );

        let detail = layout.detail;
        let detail_region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::HELP_DETAIL,
            top: detail.y,
            left: detail.x,
            width: detail.width,
            height: detail.height,
            window: 0,
            kind: PoolKind::Grid,
        };
        let detail_scroll = help.detail_scroll() as u32;
        // The detail body is the selected entry's, so a hash of its name is
        // the content version: it bumps on a selection move and on a filter
        // change that lands a different entry at the same index.
        let detail_version = {
            let mut hasher = DefaultHasher::new();
            help.selected_entry()
                .map(|entry| entry.def.name())
                .hash(&mut hasher);
            hasher.finish()
        };
        pool::emit_into(
            &mut out,
            &mut stoat.smooth_scroll,
            detail_region,
            detail_scroll as f32,
            detail_version,
            false,
            |page| {
                crate::smooth_scroll::render_help_detail_page(
                    help,
                    page,
                    theme,
                    detail.width,
                    detail.height,
                )
            },
        );
    }

    if let (Some((_, inner)), Some(popup)) = (hover_layout, stoat.pending_hover.as_ref()) {
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::HOVER,
            top: inner.y,
            left: inner.x,
            width: inner.width,
            height: inner.height,
            window: 0,
            kind: PoolKind::Grid,
        };
        let interior = inner.height.max(1) as usize;
        let half_page = (interior / 2).max(1);
        let scroll = popup
            .lines
            .len()
            .saturating_sub(interior)
            .min(popup.scroll_half_pages * half_page);
        // The body changes only when a new hover replaces this one, so a hash
        // of the line texts is the pool's content version.
        let content_version = {
            let mut hasher = DefaultHasher::new();
            stoat.theme_epoch.hash(&mut hasher);
            popup.generation.hash(&mut hasher);
            hasher.finish()
        };
        pool::emit_into(
            &mut out,
            &mut stoat.smooth_scroll,
            region,
            scroll as f32,
            content_version,
            false,
            |page| {
                crate::render::hover::render_hover_page(
                    popup,
                    page,
                    theme,
                    inner.width,
                    inner.height,
                )
            },
        );

        // Ships after the region so the terminal has the pool before it is told
        // what that pool rides.
        //
        // A sketch card rides no pool. Its box and body ride the pane through
        // their own anchors, and a pool ride composites the page's unfaded text
        // over a box the pen has not closed.
        if let (Some((host, top_rows)), HoverFrame::Modal) = (popup.anchor, popup.frame) {
            stoatty_protocol::command::encode_pool_anchor_into(
                &mut out,
                &PoolAnchorCommand {
                    pool: crate::smooth_scroll::non_pane_pool::HOVER,
                    host,
                    top_rows,
                },
            );
        }
    }

    emit_window_content(stoat, &mut out);

    if !out.is_empty() {
        let _ = apc_tx.send(out);
    }

    // The batch (regions, scrolls) has shipped, so the terminal has every pool's
    // geometry before a fill lands. Render each newly-entered plain editor page on
    // a blocking worker and deliver its fill frame through the same channel, off
    // the run loop.
    for job in async_jobs {
        match job {
            PoolFill::Editor {
                snapshot,
                pages,
                redecorate,
                pool,
                width,
                height,
                gutter,
                diff_view,
                dim,
                dials,
                spotlight,
            } => {
                // One job for the whole refill rather than one per page.
                // Every page reads the same highlight endpoints and the same
                // hunk positions, and resolving those is what a page costs
                // beyond painting, so sharing them means resolving once for
                // the run instead of once per page. Fills still ship one at
                // a time, so the terminal sees each page as it finishes.
                let apc_tx = apc_tx.clone();
                stoat
                    .executor
                    .spawn_blocking(move || {
                        // Endpoints wider than the rows a page paints are
                        // valid, so one resolve over the run's union serves
                        // every page in it.
                        let endpoints = match (pages.first(), pages.last()) {
                            (Some(&first), Some(&last)) => {
                                let top = crate::smooth_scroll::page_top_row(first, height);
                                let bottom = crate::smooth_scroll::page_top_row(last, height)
                                    .saturating_add(height as u32);
                                snapshot.highlighted_endpoints(top..bottom)
                            },
                            _ => Arc::from(Vec::new()),
                        };
                        // Resolving these walks every hunk anchor in the file
                        // and sorts the result, for an answer that does not
                        // vary by page.
                        let live = snapshot
                            .diff_map()
                            .map(|dm| dm.live_hunks(snapshot.buffer_snapshot()));

                        for index in pages {
                            let fill = crate::smooth_scroll::render_page_fill(
                                &snapshot,
                                pool,
                                index,
                                fallback_style,
                                width,
                                height,
                                &gutter,
                                diff_view,
                                dim,
                                dials,
                                endpoints.clone(),
                                live.as_ref(),
                                spotlight.as_ref(),
                            );
                            if apc_tx.send(fill).is_err() {
                                return;
                            }
                        }

                        for index in redecorate {
                            let decorations = crate::smooth_scroll::render_page_decorations(
                                &snapshot,
                                pool,
                                index,
                                width,
                                height,
                                &gutter,
                                live.as_ref(),
                            );
                            if decorations.is_empty() {
                                continue;
                            }
                            if apc_tx.send(decorations).is_err() {
                                return;
                            }
                        }
                    })
                    .detach();
            },
            PoolFill::Conflict {
                snapshot,
                parts,
                pages,
                pool,
                width,
                height,
            } => {
                let ConflictFillParts { state, theme } = *parts;
                for index in pages {
                    let snapshot = snapshot.clone();
                    let state = state.clone();
                    let theme = theme.clone();
                    let apc_tx = apc_tx.clone();
                    stoat
                        .executor
                        .spawn_blocking(move || {
                            let fill = crate::smooth_scroll::render_conflict_page_from_parts(
                                &snapshot,
                                &state,
                                &theme,
                                pool,
                                index,
                                fallback_style,
                                width,
                                height,
                            );
                            let _ = apc_tx.send(fill);
                        })
                        .detach();
                }
            },
        }
    }
}

/// Retire every pane pool whose owner changed since the last pass, and record
/// the owners of this pass.
///
/// The id of a pane pool is its pane's index, so the id stays when the
/// surface changes. A new content version does not replace the drop. The
/// terminal still composites the slots it holds and glides on from the old
/// offset, so the pages of one file show over another until the refill lands.
/// After the drop, this pass declares and fills the pool from scratch.
///
/// A pool changes owner when it gains, loses, or swaps one. A gain or a loss
/// matters for a detached pane that changes between an editor and another
/// view, because [`emit_window_content`] keeps the same id active for the
/// content pool of the other view.
///
/// The cursor record of a focused detached pane names its pool. A drop clears
/// that record, so the pass ships the cursor to the declared pool.
fn drop_reowned_pools(
    stoat: &mut Stoat,
    out: &mut Vec<u8>,
    panes: &[(u32, EditorId, PoolRegionCommand)],
    terminal_panes: &[TerminalPoolPane],
) {
    let workspace = stoat.active_workspace;
    let owners: std::collections::BTreeMap<u32, PoolOwner> = panes
        .iter()
        .map(|(pool, editor, _)| (*pool, PoolOwner::Editor(workspace, *editor)))
        .chain(terminal_panes.iter().map(|pane| {
            (
                pane.region.pool,
                PoolOwner::Terminal(workspace, pane.term_id),
            )
        }))
        .collect();

    let held = std::mem::take(&mut stoat.pool_owners);
    let lost_or_swapped = held
        .iter()
        .filter(|(pool, owner)| owners.get(pool) != Some(owner))
        .map(|(pool, _)| *pool);
    let gained = owners
        .keys()
        .filter(|pool| !held.contains_key(pool))
        .copied();
    for pool in lost_or_swapped.chain(gained) {
        stoat.smooth_scroll.drop_pool(out, pool);
        if stoat
            .aux_cursor
            .is_some_and(|(cursor_pool, _, _)| cursor_pool == pool)
        {
            stoat.aux_cursor = None;
        }
    }

    stoat.pool_owners = owners;
}

/// Every visible split pane showing an editor, as `(pool id, editor id,
/// pool region)`.
///
/// One entry per [`Placement::Split`] pane whose [`View::Editor`] has a
/// non-empty content area, plain or review alike. The pool id is the pane's
/// stable [`crate::pane::Pane::index`], so a pane keeps its pool across
/// frames; the region is the pane area minus its bottom status row, the same
/// content area the editor is painted into. The caller pools nothing while a
/// full-screen overlay mode is active.
pub(crate) fn editor_pool_panes(stoat: &Stoat) -> Vec<(u32, EditorId, PoolRegionCommand)> {
    let ws = stoat.active_workspace();
    ws.panes
        .split_panes()
        .filter_map(|(_, pane)| {
            if pane.placement != Placement::Split {
                return None;
            }
            let View::Editor(editor_id) = pane.view else {
                return None;
            };
            let editor = ws.editors.get(editor_id)?;

            let (content, _) = crate::render::layout::split_pane_status(pane.area);
            if content.width == 0 || content.height == 0 {
                return None;
            }

            // The pool composite paints an opaque quad per cell across the
            // region during a glide, which would bury the right-edge minimap
            // strip and thumb drawn in the base pass. Exclude the strip
            // columns from the region. minimap_rect is Some exactly when the
            // strip is drawn, so its width is the columns to reserve.
            let strip_cols = editor.minimap_rect.map_or(0, |rect| rect.width);
            let width = content.width.saturating_sub(strip_cols);
            if width == 0 {
                return None;
            }

            Some((
                pane.index,
                editor_id,
                PoolRegionCommand {
                    pool: pane.index,
                    top: content.y,
                    left: content.x,
                    width,
                    height: content.height,
                    window: 0,
                    kind: PoolKind::Grid,
                },
            ))
        })
        .collect()
}

/// Every visible split pane showing a terminal or an agent, as the pool that
/// draws it at the terminal font size.
///
/// The pool id is the pane's stable index, as an editor pane's is, so a pane
/// that changes view keeps its id and the terminal declares it again as the
/// other kind. The region is the pane's content area, which the pool covers
/// whatever count of terminal cells it holds.
fn terminal_pool_panes(stoat: &Stoat, inactive_dim: f32) -> Vec<TerminalPoolPane> {
    let ws = stoat.active_workspace();
    let focused = matches!(ws.focus, FocusTarget::SplitPane).then(|| ws.panes.focus());
    ws.panes
        .split_panes()
        .filter_map(|(id, pane)| {
            if pane.placement != Placement::Split {
                return None;
            }
            let (View::Terminal(term_id) | View::Agent(term_id)) = pane.view else {
                return None;
            };
            ws.terms.get(term_id)?;

            let (content, _) = crate::render::layout::split_pane_status(pane.area);
            if content.width == 0 || content.height == 0 {
                return None;
            }

            let is_focused = focused == Some(id);
            Some(TerminalPoolPane {
                term_id,
                region: PoolRegionCommand {
                    pool: pane.index,
                    top: content.y,
                    left: content.x,
                    width: content.width,
                    height: content.height,
                    window: 0,
                    kind: PoolKind::Terminal,
                },
                is_focused,
                dim: match is_focused {
                    true => 0.0,
                    false => inactive_dim,
                },
            })
        })
        .collect()
}

/// Declare `pane`'s pool and fill its one page with `term` drawn at `grid`, the
/// `(rows, cols)` stoatty reported for it.
///
/// A pane whose grid has not been reported yet declares its region alone,
/// which is what has stoatty report it. A pane whose inputs all held since its
/// last emit sends nothing.
fn emit_terminal_pool(
    out: &mut Vec<u8>,
    state: &mut SmoothScrollState,
    term: &TermSession,
    grid: Option<(u16, u16)>,
    pane: &TerminalPoolPane,
    theme: &crate::theme::Theme,
    theme_epoch: u64,
) {
    let version = {
        let mut hasher = DefaultHasher::new();
        theme_epoch.hash(&mut hasher);
        pane.region.hash(&mut hasher);
        grid.hash(&mut hasher);
        pane.is_focused.hash(&mut hasher);
        pane.dim.to_bits().hash(&mut hasher);
        term.term.generation().hash(&mut hasher);
        term.selection.hash(&mut hasher);
        hasher.finish()
    };
    if state.already_emitted(pane.region.pool, version) {
        return;
    }

    let page = grid.map(|(rows, cols)| terminal_page(term, theme, rows, cols, pane));
    pool::emit_into(
        out,
        state,
        pane.region,
        0.0,
        version,
        false,
        |index| match (index, &page) {
            (0, Some(bytes)) => bytes.clone(),
            _ => Vec::new(),
        },
    );
}

/// `term`'s screen drawn into a `rows` by `cols` page as the live grid draws
/// the pane, faded by the pane's dim.
fn terminal_page(
    term: &TermSession,
    theme: &crate::theme::Theme,
    rows: u16,
    cols: u16,
    pane: &TerminalPoolPane,
) -> Vec<u8> {
    let area = Rect::new(0, 0, cols, rows);
    let mut buf = crate::smooth_scroll::page_buffer(area, theme);
    crate::render::term_pane::render_term_pane(term, theme, area, pane.is_focused, &mut buf);
    if pane.dim > 0.0
        && let Some(bg) = crate::render::paint::style_rgb(
            theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|style| style.bg),
        )
    {
        crate::render::pane::dim_pane_content(&mut buf, area, bg, pane.dim);
    }
    crate::render::serialize_buffer(&buf)
}

/// The detached editor panes and their window-bound pool regions.
///
/// The counterpart to [`editor_pool_panes`] over the workspace's
/// windowed panes. Each region's coordinates are relative to the pane's own
/// aux window (`window` is nonzero), and no minimap-strip columns are
/// reserved since aux windows draw no minimap.
fn windowed_editor_pool_panes(stoat: &Stoat) -> Vec<(u32, EditorId, PoolRegionCommand)> {
    let ws = stoat.active_workspace();
    ws.panes
        .windowed_panes()
        .into_iter()
        .filter_map(|(pane_id, window)| {
            let pane = ws.panes.pane(pane_id);
            let View::Editor(editor_id) = pane.view else {
                return None;
            };
            ws.editors.get(editor_id)?;

            let (content, _) = crate::render::layout::split_pane_status(pane.area);
            if content.width == 0 || content.height == 0 {
                return None;
            }

            Some((
                pane.index,
                editor_id,
                PoolRegionCommand {
                    pool: pane.index,
                    top: content.y,
                    left: content.x,
                    width: content.width,
                    height: content.height,
                    window,
                    kind: PoolKind::Grid,
                },
            ))
        })
        .collect()
}

/// OSC 110 and OSC 111, restoring the terminal's own default foreground and
/// background after [`osc_default_colors`] overrode them.
const OSC_RESET_DEFAULT_COLORS: &[u8] = b"\x1b]110\x1b\\\x1b]111\x1b\\";

/// OSC 10 and OSC 11 setting the hosting terminal's default foreground and
/// background to `theme`'s own.
///
/// A channel the theme leaves undefined or non-RGB is skipped rather than
/// guessed at, so a partial theme overrides only what it actually specifies and
/// leaves the rest to the terminal. Both missing yields empty bytes.
pub(crate) fn osc_default_colors(theme: &crate::theme::Theme) -> Vec<u8> {
    use crate::{render::paint::style_rgb, theme::scope};

    let mut out = Vec::new();
    let mut push = |code: u8, rgb: Option<[u8; 3]>| {
        if let Some([r, g, b]) = rgb {
            out.extend_from_slice(format!("\x1b]{code};#{r:02x}{g:02x}{b:02x}\x1b\\").as_bytes());
        }
    };

    push(
        10,
        style_rgb(theme.try_get(scope::UI_TEXT).and_then(|s| s.fg)),
    );
    push(
        11,
        style_rgb(theme.try_get(scope::UI_BACKGROUND).and_then(|s| s.bg)),
    );
    out
}

/// Content version of a detached pane's content rows, hashing what its render
/// reads, or [`None`] for a view whose sources are not tracked.
///
/// `None` means the caller has to paint the pane and hash the result to learn
/// whether anything moved, which is what every view did before any of them
/// carried a counter. A version answers the same question without painting, so
/// a pane sitting idle in an aux window costs a hash rather than a render, a
/// buffer copy, and a serialize per frame.
///
/// The region belongs in the hash even though nothing inside the pane draws it.
/// Acting on a match skips [`pool::emit_into`] altogether, and with it the
/// region declaration that a resize or a move to another window otherwise
/// emits.
pub(crate) fn window_content_version(
    view: &View,
    region: PoolRegionCommand,
    is_focused: bool,
    theme_epoch: u64,
    ws: &Workspace,
) -> Option<u64> {
    let mut hasher = DefaultHasher::new();
    theme_epoch.hash(&mut hasher);
    region.hash(&mut hasher);
    is_focused.hash(&mut hasher);

    match view {
        // The leading tag keeps one view kind's inputs from aliasing another's
        // after a pane changes view but keeps its index, and so its pool.
        View::Agent(term_id) | View::Terminal(term_id) => {
            let term = ws.terms.get(*term_id)?;
            0u8.hash(&mut hasher);
            term_id.hash(&mut hasher);
            term.term.generation().hash(&mut hasher);
            term.selection.hash(&mut hasher);
        },
        View::Run(run_id) => {
            let run = ws.runs.get(*run_id)?;
            1u8.hash(&mut hasher);
            run_id.hash(&mut hasher);
            run.scroll_offset.hash(&mut hasher);
            run.cwd.hash(&mut hasher);
            run.blocks.len().hash(&mut hasher);
            for block in &run.blocks {
                // Only the grid needs a counter. Every other field the block
                // renders is a scalar or a short string, cheap enough to hash
                // outright and safer than tracking who writes it.
                block.grid.generation().hash(&mut hasher);
                block.command.hash(&mut hasher);
                block.cwd.hash(&mut hasher);
                block.finished.hash(&mut hasher);
                block.exit_status.hash(&mut hasher);
                block.error.hash(&mut hasher);
                block.selection.hash(&mut hasher);
            }
            input_view_version(&run.input, ws, &mut hasher)?;
        },
        // An image pane paints one label, so it feeds no pool. A commits list
        // pools its rows as a surface of its own rather than as a pane.
        View::Editor(_) | View::Label(_) | View::Image { .. } | View::Commits(_) => return None,
    }

    Some(hasher.finish())
}

/// Fold an input line's rendered state into `hasher`, or return [`None`] when
/// its editor or buffer is gone and the version cannot be trusted.
///
/// The line is an editor over a one-row buffer, so what it draws moves on a
/// text edit, a cursor move, and a horizontal scroll. The buffer version, the
/// selection anchors, and the scroll row cover those in turn.
///
/// Hashing raw anchors rather than the offsets they resolve to is enough. An
/// edit can move where an anchor lands without changing the anchor itself, but
/// it bumps the buffer version in the same breath.
fn input_view_version(input: &InputView, ws: &Workspace, hasher: &mut DefaultHasher) -> Option<()> {
    let editor = ws.editors.get(input.editor_id)?;
    editor.scroll_row.hash(hasher);
    for selection in editor.selections.all_anchors() {
        selection.start.hash(hasher);
        selection.end.hash(hasher);
    }

    let buffer = ws.buffers.get(editor.buffer_id)?;
    let version = buffer.read().ok()?.version();
    version.hash(hasher);
    Some(())
}

/// One scalar standing for the text a frame paints, for the caches that compare
/// numbers rather than snapshots.
///
/// The buffer version alone misses the display layers, which reflow the same
/// text into different rows, and the paint version alone misses the edits that
/// reach no layer. Together they answer what a page cache asks: whether this
/// frame lays the rows out the way the held one did.
pub(crate) fn display_map_stamp(buffer_version: u64, paint_version: PaintVersion) -> u64 {
    let mut hasher = DefaultHasher::new();
    buffer_version.hash(&mut hasher);
    paint_version.hash(&mut hasher);
    hasher.finish()
}

/// Content version of a pooled editor page, hashing the inputs whose change
/// forces a buffered page to refill.
///
/// A page stays cached while the surface scrolls, but must repaint when the
/// syntax-highlight toggle recolors every row, a diagnostics change restyles
/// the gutter, a gutter-width or wrap-width change reflows the text, the
/// cursor's buffer line moves under relative numbering, any of the diff
/// view's dials moves, the theme changes every color on the page, or the
/// walkthrough spotlight's dim comes on, goes out, or changes.
///
/// Where the spotlight lights is no part of this. It recolors only the rows it
/// lights, so [`spotlight_page_version`] carries it in each page's own
/// version, and a move between two annotations refills only the pages either
/// one lights.
#[allow(clippy::too_many_arguments)]
pub(crate) fn editor_page_content_version(
    syntax_highlight: bool,
    gutter_width: u16,
    wrap_width: Option<u32>,
    current_line: Option<u32>,
    severity_version: u64,
    diff_view: bool,
    diff_version: usize,
    dim: f32,
    dials: DiffDials,
    buffer_version: u64,
    paint_version: PaintVersion,
    theme_epoch: u64,
    spotlight: Option<&Spotlight>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    (!syntax_highlight).hash(&mut hasher);
    gutter_width.hash(&mut hasher);
    wrap_width.hash(&mut hasher);
    current_line.hash(&mut hasher);
    severity_version.hash(&mut hasher);
    diff_view.hash(&mut hasher);
    diff_version.hash(&mut hasher);
    ((dim * 1000.0) as u32).hash(&mut hasher);
    ((dials.soften_scale * 1000.0) as u32).hash(&mut hasher);
    ((dials.tint_amount * 1000.0) as u32).hash(&mut hasher);
    dials.bold.hash(&mut hasher);
    dials.underline.hash(&mut hasher);
    // A typed character and a fold both change page pixels and reach nothing
    // else here, so without the mapping stamp a file outside git (diff_version
    // stuck at 0) with no diagnostics glides pre-edit text.
    display_map_stamp(buffer_version, paint_version).hash(&mut hasher);
    theme_epoch.hash(&mut hasher);
    spotlight
        .map(|spotlight| (spotlight.dim * 1000.0) as u32)
        .hash(&mut hasher);
    hasher.finish()
}

/// The cells version the walkthrough spotlight gives editor page `index` of a
/// pool whose regions are `height` rows tall.
///
/// `lit` is the spotlight's color and display span. The version hashes the
/// color with the part of the span the page holds, so a page the span misses
/// answers 0, the same as a page with no spotlight at all.
fn spotlight_page_version(
    lit: Option<([u8; 3], (DisplayPoint, DisplayPoint))>,
    index: u64,
    height: u16,
) -> u64 {
    let Some((color, (start, end))) = lit else {
        return 0;
    };

    let top_row = crate::smooth_scroll::page_top_row(index, height);
    let page_end = top_row.saturating_add(u32::from(height));
    if end.row < top_row || start.row >= page_end {
        return 0;
    }

    let from = match start.row >= top_row {
        true => (start.row, start.column),
        false => (top_row, 0),
    };
    let to = match end.row < page_end {
        true => (end.row, end.column),
        false => (page_end, 0),
    };

    let mut hasher = DefaultHasher::new();
    (color, from, to).hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests;
