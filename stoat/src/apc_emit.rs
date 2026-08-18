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
    display_map::{DisplaySnapshot, PaintVersion},
    editor_state::{EditorId, ScrollGlide},
    input_view::InputView,
    minimap::emit::minimap_view_window,
    pane::{FocusTarget, Placement, View},
    render::undercurl::UndercurlBatch,
    workspace::Workspace,
};
use ratatui::{buffer::Buffer, layout::Rect};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::Arc,
};
use stoat_config::{LineNumbers, WrapMode};
use stoat_widgets::{
    pool::{self, MinimapWindowInputs},
    ApcScene,
};
use stoatty_protocol::command::{PoolAnchorCommand, PoolRegionCommand};

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
/// While claimed the terminal forwards each press as a
/// [`WindowIpcEvent::Zoom`] instead of stepping its own font size, which is
/// what lets the combo mean whatever the current context calls for.
///
/// [`Stoat::sync_zoom_claim`] decides when to call this and holds the reason.
/// A claim outlives no more than the window socket carrying the presses
/// back, so it is not a once-per-session send.
///
/// A no-op until [`Stoat::stoatty`] confirms a listener, since under any
/// other terminal the combo never reaches stoat at all and that terminal
/// keeps its own font zoom.
pub(crate) fn emit_zoom_capture(stoat: &Stoat, on: bool) {
    let Some(apc_tx) = stoat.apc_tx.clone().filter(|_| stoat.stoatty) else {
        return;
    };

    let mut out = Vec::new();
    stoatty_protocol::command::encode_zoom_capture_into(&mut out, on);
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
    stoat.refresh_chrome();
    let ws = &mut stoat.workspaces[stoat.active_workspace];
    let windowed = ws.panes.windowed_panes();
    if windowed.is_empty() {
        return;
    }

    let workspace_name = if !ws.name.is_empty() {
        ws.name.clone()
    } else {
        ws.git_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("(unnamed)")
            .to_string()
    };
    let screen = crate::keymap_state::view_predicate(ws);
    let focus_target = ws.focus;
    let focus_id = ws.panes.focus();

    let frame = crate::render::FrameCtx {
        workspace_name: &workspace_name,
        workspace_root: &ws.git_root,
        mode: &mode,
        screen,
        theme: &stoat.theme,
        chrome: &stoat.chrome.as_ref().expect("refresh_chrome ran above").1,
        pending_count: stoat.pending_count,
        recording_register: stoat
            .macro_recording
            .as_ref()
            .map(|rec| rec.register.name()),
        lsp_status_open: false,
        lsp_progress_entries: &[],
        spinner_phase: 0,
        lsp_servers: &[],
        diff_warm_busy: false,
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
        minimap_enabled: false,
        minimap_chrome: None,
        minimap_band: None,
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
/// Runs at the frame seam after the live frame is published, so the pane
/// layout (and thus each editor rectangle) reflects the frame just drawn and
/// the APC bytes are written to stdout right after the grid frame.
pub(crate) fn emit_smooth_scroll(stoat: &mut Stoat) {
    let Some(apc_tx) = stoat.apc_tx.clone().filter(|_| stoat.stoatty) else {
        return;
    };
    stoat.refresh_chrome();

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

    // The commits overlay renders into the focused pane; its left list pools
    // as a non-pane surface while editor panes stay suppressed in this mode.
    let commits_region = (stoat.focused_mode() == "commits")
        .then(|| {
            let ws = stoat.active_workspace();
            ws.commits.as_ref()?;
            let pane = ws.panes.pane(ws.panes.focus());
            crate::render::commits::commits_list_rect(pane.area)
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
                .is_some_and(|c| picker.preview_sessions.get(&c.sha).is_some())
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

    let mut out = Vec::new();
    let mut active: Vec<u32> = panes.iter().map(|(pool, _, _)| *pool).collect();
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
    // The review view and theme a pooled review page needs, boxed so the
    // Review variant does not dwarf Editor.
    struct ReviewFillParts {
        view: Arc<crate::review_session::ReviewViewState>,
        theme: Arc<crate::theme::Theme>,
    }
    struct ConflictFillParts {
        state: Arc<crate::conflict_session::ConflictViewState>,
        theme: Arc<crate::theme::Theme>,
    }
    enum PoolFill {
        Editor {
            snapshot: DisplaySnapshot,
            pages: Vec<u64>,
            pool: u32,
            width: u16,
            height: u16,
            gutter: crate::smooth_scroll::PageGutter,
            diff_view: bool,
            dim: f32,
        },
        Review {
            snapshot: DisplaySnapshot,
            parts: Box<ReviewFillParts>,
            pages: Vec<u64>,
            pool: u32,
            width: u16,
            height: u16,
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
    let syntax_highlight = stoat.syntax_highlight;
    let line_numbers = stoat
        .settings
        .editor_line_numbers
        .unwrap_or(LineNumbers::Relative);
    let inactive_dim = stoat
        .settings
        .ui_inactive_dim
        .unwrap_or(0.25)
        .clamp(0.0, 1.0) as f32;
    // Relative numbering follows the same pane the live render calls focused:
    // the focused split editor outside insert mode. Resolved before the ws
    // borrow so the per-pane loop can gate on it.
    let focused_editor = stoat.focused_editor_ids().map(|(id, _)| id);
    let focused_insert = stoat.focused_mode() == "insert";
    let single_minimap = stoat.single_minimap_rect.is_some();
    // The focused detached pane's pool cursor, collected during the pane loop
    // and emitted after it, past the workspace borrow.
    let mut detached_cursor: Option<(u32, u64, u16)> = None;
    let theme_epoch = stoat.theme_epoch;
    let ws = &mut stoat.workspaces[stoat.active_workspace];
    let theme = &stoat.theme;
    let fallback_style = theme.get(crate::theme::scope::UI_TEXT);
    // Resolved once for the theme rather than per pane per frame.
    let base_rich = stoat
        .chrome
        .as_ref()
        .and_then(|(_, chrome)| chrome.rich_gutter.clone());
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
        // Relative numbers reference the cursor's buffer line, which the
        // wheel glide's cursor-follow drags every row. Holding the baked line
        // steady through the glide keeps the content version stable so the
        // window does not refill per dragged row. The settle emit recomputes
        // the live line and refills the window once to match the repainted
        // grid.
        let current_line = if editor.scroll_glide != ScrollGlide::None {
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

        // Version-cached, so the extra query is cheap. The pair carries every
        // edit and every layer reflow into the content hash below.
        let buffer_version = editor.display_map.buffer_snapshot().version();
        let paint_version = editor.display_map.snapshot().paint_version();
        let content_version = match editor.review_view.as_ref() {
            Some(view) => view.session_version,
            None => editor_page_content_version(
                syntax_highlight,
                editor.gutter_width,
                editor.display_map.wrap_width(),
                current_line,
                editor
                    .gutter_severity_cache
                    .as_ref()
                    .map_or(0, |cache| cache.version),
                editor.diff_view,
                editor.display_map.diff_version(),
                dim,
                buffer_version,
                paint_version,
                theme_epoch,
            ),
        };
        let entered = pool::emit_into(
            &mut out,
            &mut stoat.smooth_scroll,
            region,
            scroll_offset,
            content_version,
            // Editor panes refill constantly at rest for the cursor line,
            // focus dim, and diagnostics, so they hold the window until the
            // glide starts.
            true,
            // Editor and review pages both fill asynchronously below, so the
            // synchronous render emits nothing here.
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
        if region.window == 0
            && focused_editor == Some(*editor_id)
            && editor.scroll_glide != ScrollGlide::None
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
        }

        // A focused detached pane draws its cursor from its window pool. No
        // live paint records its screen cell, so derive the display cell
        // directly. The actual emit is change-gated after the loop.
        if region.window != 0 && focused_editor == Some(*editor_id) {
            let (row, column) = view::cursor_display_cell(editor);
            let col = region.left + editor.gutter_width + column as u16;
            detached_cursor = Some((region.pool, row as u64, col));
        }

        if !entered.is_empty() {
            let snapshot = editor.display_map.snapshot();
            if let Some(view) = editor.review_view.as_ref() {
                async_jobs.push(PoolFill::Review {
                    snapshot,
                    parts: Box::new(ReviewFillParts {
                        view: view.clone(),
                        theme: theme.clone(),
                    }),
                    pages: entered,
                    pool: region.pool,
                    width: region.width,
                    height: region.height,
                });
            } else if let Some(state) = editor.conflict_view.as_ref() {
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
                    dim,
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

    if let (Some(list), Some(finder)) = (finder_list, stoat.file_finder.as_ref()) {
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::FINDER,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
            window: 0,
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
        stoat.workspaces[stoat.active_workspace].commits.as_ref(),
    ) {
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::COMMITS,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
            window: 0,
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
        && let Some(session) = picker
            .selected_commit()
            .and_then(|c| picker.preview_sessions.get(&c.sha))
            .cloned()
    {
        let region = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::COMMIT_PICKER_PREVIEW,
            top: rect.y,
            left: rect.x,
            width: rect.width,
            height: rect.height,
            window: 0,
        };
        // Clamped through the same helper the renderer uses, so the pool
        // scrolls to the row the diff actually lands on.
        let scroll_row = crate::render::commit_picker::clamped_preview_scroll(
            picker.preview_scroll,
            &session,
            rect.height,
        );
        // A different commit means a different diff, and a rebuilt session
        // changes its length, so both refill the pages.
        let content_version = {
            let mut hasher = DefaultHasher::new();
            stoat.theme_epoch.hash(&mut hasher);
            picker
                .selected_commit()
                .map(|c| c.sha.as_str())
                .hash(&mut hasher);
            crate::render::commits::preview_row_count(&session).hash(&mut hasher);
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
                crate::smooth_scroll::render_commit_picker_preview_page(
                    &session,
                    page,
                    theme,
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
                    &popup.items,
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
        if let Some((host, top_rows)) = popup.anchor {
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
                pool,
                width,
                height,
                gutter,
                diff_view,
                dim,
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
                                endpoints.clone(),
                                live.as_ref(),
                            );
                            if apc_tx.send(fill).is_err() {
                                return;
                            }
                        }
                    })
                    .detach();
            },
            PoolFill::Review {
                snapshot,
                parts,
                pages,
                pool,
                width,
                height,
            } => {
                let ReviewFillParts { view, theme } = *parts;
                for index in pages {
                    let snapshot = snapshot.clone();
                    let view = view.clone();
                    let theme = theme.clone();
                    let apc_tx = apc_tx.clone();
                    stoat
                        .executor
                        .spawn_blocking(move || {
                            let fill = crate::smooth_scroll::render_review_page_from_parts(
                                &snapshot,
                                &view,
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
                },
            ))
        })
        .collect()
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
        View::Editor(_) | View::Label(_) => return None,
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
/// cursor's buffer line moves under relative numbering, or the theme changes
/// every color on the page.
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
    buffer_version: u64,
    paint_version: PaintVersion,
    theme_epoch: u64,
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
    // A typed character and a fold both change page pixels and reach nothing
    // else here, so without the mapping stamp a file outside git (diff_version
    // stuck at 0) with no diagnostics glides pre-edit text.
    display_map_stamp(buffer_version, paint_version).hash(&mut hasher);
    theme_epoch.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action_handlers::movement;

    /// The rich review gutter engages only when every color resolves to RGB, so
    /// tests need a hex theme. The default theme uses named colors.
    fn rgb_review_theme() -> crate::theme::Theme {
        let src = r##"theme rgbtest {
            diff.context.fg = "#808080";
            diff.added.fg = "#00ff00";
            diff.deleted.fg = "#ff0000";
            diff.current_hunk.fg = "#00ffff";
            ui.text.muted.fg = "#606060";
            ui.background.bg = "#282c34";
        }"##;
        let (config, _) = stoat_config::parse(src);
        crate::theme::Theme::from_config(&config.expect("theme config parses"), "rgbtest")
            .expect("rgb theme builds")
    }

    fn rgb_diagnostic_theme() -> crate::theme::Theme {
        let src = r##"theme rgbdiag {
            ui.diagnostic.error.fg = "#ff0000";
            ui.diagnostic.warning.fg = "#ffff00";
            ui.diagnostic.info.fg = "#00ffff";
            ui.diagnostic.hint.fg = "#808080";
            ui.text.muted.fg = "#606060";
            ui.background.bg = "#282c34";
        }"##;
        let (config, _) = stoat_config::parse(src);
        crate::theme::Theme::from_config(&config.expect("theme config parses"), "rgbdiag")
            .expect("rgb theme builds")
    }

    /// modal_frame's rich arm engages only when the border fg and the mask bg
    /// both resolve to RGB, so the modal APC tests need a hex theme. The default
    /// theme uses named colors and would fall back to glyphs.
    fn rgb_modal_theme() -> crate::theme::Theme {
        let src = r##"theme rgbmodal {
            ui.modal.help.fg = "#8899aa";
            ui.modal.hints.fg = "#8899aa";
            ui.text.fg = "#c8ccd4";
            ui.text.muted.fg = "#606060";
            ui.border.inactive.fg = "#606060";
            ui.key_label.fg = "#d19a66";
            ui.background.bg = "#282c34";
        }"##;
        let (config, _) = stoat_config::parse(src);
        crate::theme::Theme::from_config(&config.expect("theme config parses"), "rgbmodal")
            .expect("rgb theme builds")
    }

    /// A hex theme for the pane-divider APC test, so the border colors resolve
    /// to RGB and the stoatty arm emits bars instead of glyphs.
    fn rgb_border_theme() -> crate::theme::Theme {
        let src = r##"theme rgbborder {
            ui.border.focused.fg = "#aabbcc";
            ui.border.inactive.fg = "#556677";
        }"##;
        let (config, _) = stoat_config::parse(src);
        crate::theme::Theme::from_config(&config.expect("theme config parses"), "rgbborder")
            .expect("rgb theme builds")
    }

    use crate::{
        action_handlers, app,
        term_session::TermSession,
        test_fixture::{
            drain_apc, finder_layout, help_layout, open_with_minimap_strip, palette_sizing,
        },
    };
    use std::path::PathBuf;
    use stoat_action::{Conflict, OpenFile};
    use stoat_config::MinimapMode;
    use stoatty_protocol::{command, window_ipc::WindowIpcEvent};

    /// A fill frame carries raw ANSI between its APC markers, so a terminal that
    /// drops the markers prints the payload over the screen. Nothing may reach
    /// the wire before a listener is confirmed.
    #[test]
    fn a_frame_emits_nothing_until_a_stoatty_is_confirmed() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.stoatty = false;

        let root = PathBuf::from("/gate");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        emit_smooth_scroll(&mut h.stoat);
        emit_apc_scene(&mut h.stoat);
        assert_eq!(
            drain_apc(&mut rx),
            Vec::new(),
            "no pool is declared, so nothing is left needing a drop either"
        );

        h.stoat.stoatty = true;
        emit_smooth_scroll(&mut h.stoat);
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|cmd| matches!(cmd, Command::PoolRegion(_))),
            "and the same frame declares the editor pool once one answers, got {cmds:?}"
        );
    }

    #[test]
    fn detach_emits_window_open_and_reattach_emits_window_close() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.window_ipc_connected = true;

        let path = PathBuf::from("/w/a.txt");
        h.fake_fs().insert_file(&path, b"hello\n");
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.resize(80, 24);
        h.type_action("SplitRight()");
        h.settle();

        h.type_action("DetachPane()");
        emit_windows(&mut h.stoat);
        let opened = drain_apc(&mut rx);
        assert!(
            opened.iter().any(|c| matches!(c, Command::WindowOpen(_))),
            "detach opens a window, got {opened:?}"
        );

        emit_windows(&mut h.stoat);
        assert!(
            drain_apc(&mut rx).is_empty(),
            "an unchanged frame emits no window commands"
        );

        h.type_action("ReattachPane()");
        emit_windows(&mut h.stoat);
        let closed = drain_apc(&mut rx);
        assert!(
            closed.iter().any(|c| matches!(c, Command::WindowClose(_))),
            "reattach closes the window, got {closed:?}"
        );
    }

    #[test]
    fn editor_pool_pane_region_is_content_rect() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/pool");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(2, 1, 76, 23);

        let panes = editor_pool_panes(&h.stoat);
        assert_eq!(panes.len(), 1, "one editor pane is pooled");
        let (_, _, region) = panes[0];
        // Content rect is the pane area minus its one-row status bar.
        assert_eq!(
            (region.top, region.left, region.width, region.height),
            (1, 2, 76, 22)
        );
    }

    #[test]
    fn editor_pool_pane_region_excludes_the_minimap_strip() {
        let mut h = Stoat::test();
        let editor_id = open_with_minimap_strip(&mut h);

        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(2, 1, 76, 23);

        let (_, _, region) = editor_pool_panes(&h.stoat)[0];
        // The 8-column strip is reserved so a glide's pool composite cannot paint
        // over it, leaving content width 76 minus the strip's 8 columns.
        assert_eq!(
            (region.top, region.left, region.width, region.height),
            (1, 2, 68, 22)
        );

        h.stoat.active_workspace_mut().editors[editor_id].minimap_rect = None;
        let (_, _, region) = editor_pool_panes(&h.stoat)[0];
        assert_eq!(region.width, 76, "no strip restores the full content width");
    }

    #[test]
    fn no_pool_pane_when_pane_is_not_an_editor() {
        let mut h = Stoat::test();
        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).view = View::Label("scratch".into());
        assert!(editor_pool_panes(&h.stoat).is_empty());
    }

    #[test]
    fn emit_smooth_scroll_retires_pools_in_overlay_mode() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/pool");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        emit_smooth_scroll(&mut h.stoat);
        let first = drain_apc(&mut rx);
        assert!(
            first
                .iter()
                .any(|cmd| matches!(cmd, Command::PoolRegion(_))),
            "first emit declares the editor pool, got {first:?}"
        );

        // Entering a full-screen overlay screen retires the editor pool.
        h.stoat.active_workspace_mut().rebase = Some(crate::rebase::RebaseState::new(
            PathBuf::from("/pool"),
            "onto".into(),
            vec![],
        ));
        emit_smooth_scroll(&mut h.stoat);
        let cmds = drain_apc(&mut rx);
        assert!(
            !cmds.is_empty() && cmds.iter().all(|cmd| matches!(cmd, Command::PoolDrop(_))),
            "overlay mode only drops pools, got {cmds:?}"
        );
    }

    #[test]
    fn emit_smooth_scroll_pools_the_hover_and_retires_it_on_close() {
        use crate::render::hover::HoverPopup;
        use ratatui::style::Style;
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/pool");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        h.stoat.pending_hover = Some(HoverPopup::new(
            vec![vec![("hovered".to_string(), Style::default())]],
            0,
            editor_id,
        ));
        emit_smooth_scroll(&mut h.stoat);
        let opened = drain_apc(&mut rx);
        assert!(
            opened.iter().any(|cmd| matches!(
                cmd,
                Command::PoolRegion(r) if r.pool == crate::smooth_scroll::non_pane_pool::HOVER
            )),
            "an open hover emits its pool region, got {opened:?}"
        );

        h.stoat.pending_hover = None;
        emit_smooth_scroll(&mut h.stoat);
        let closed = drain_apc(&mut rx);
        assert!(
            closed.iter().any(|cmd| matches!(
                cmd,
                Command::PoolDrop(d) if d.pool == crate::smooth_scroll::non_pane_pool::HOVER
            )),
            "closing the hover retires its pool, got {closed:?}"
        );
    }

    #[test]
    fn a_live_hover_selection_retires_the_pool() {
        use crate::render::hover::{HoverPopup, HoverSelection};
        use ratatui::style::Style;
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        let root = PathBuf::from("/pool");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        h.stoat.pending_hover = Some(HoverPopup::new(
            vec![vec![("hovered".to_string(), Style::default())]],
            0,
            editor_id,
        ));
        emit_smooth_scroll(&mut h.stoat);
        let opened = drain_apc(&mut rx);
        assert!(
            opened.iter().any(|cmd| matches!(
                cmd,
                Command::PoolRegion(r) if r.pool == crate::smooth_scroll::non_pane_pool::HOVER
            )),
            "an unselected hover pools its body, got {opened:?}",
        );

        if let Some(popup) = h.stoat.pending_hover.as_mut() {
            popup.selection = Some(HoverSelection {
                anchor: (0, 0),
                head: (0, 3),
                dragging: false,
            });
        }
        emit_smooth_scroll(&mut h.stoat);
        let dropped = drain_apc(&mut rx);
        assert!(
            dropped.iter().any(|cmd| matches!(
                cmd,
                Command::PoolDrop(d) if d.pool == crate::smooth_scroll::non_pane_pool::HOVER
            )),
            "a live selection retires the pool so the live frame owns it, got {dropped:?}",
        );
    }

    /// A hover harness with the popup open and rendered once, so the stamped
    /// rects and anchor are what a live frame left behind.
    fn hover_harness() -> (
        crate::test_harness::TestHarness,
        tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        EditorId,
    ) {
        use crate::render::hover::HoverPopup;
        use ratatui::style::Style;

        let mut h = Stoat::test();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        let root = PathBuf::from("/pool");
        let path = root.join("a.txt");
        h.fake_fs()
            .insert_file(&path, b"alpha\nbravo\ncharlie\ndelta\necho\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        h.stoat.pending_hover = Some(HoverPopup::new(
            vec![vec![("hovered".to_string(), Style::default())]],
            0,
            editor_id,
        ));
        h.snapshot();
        (h, rx, editor_id)
    }

    #[test]
    fn a_hover_inside_its_pane_ships_the_pool_anchor() {
        use stoatty_protocol::command::Command;

        let (mut h, mut rx, _) = hover_harness();
        let stamped = h
            .stoat
            .pending_hover
            .as_ref()
            .expect("popup")
            .anchor
            .expect("a popup inside the pane stamps an anchor");

        emit_smooth_scroll(&mut h.stoat);
        let batch = drain_apc(&mut rx);

        assert!(
            batch.iter().any(|cmd| matches!(
                cmd,
                Command::PoolAnchor(a)
                    if a.pool == crate::smooth_scroll::non_pane_pool::HOVER
                        && (a.host, a.top_rows) == stamped
            )),
            "the hover pool names the pane it rides, got {batch:?}"
        );
    }

    #[test]
    fn a_hover_overflowing_its_pane_ships_no_anchor() {
        use crate::render::hover::HoverPopup;
        use ratatui::style::Style;
        use stoatty_protocol::command::Command;

        let (mut h, mut rx, _) = hover_harness();
        // The popup is window-bounded, not pane-bounded, so a line wider than
        // half the terminal overflows a vertically split pane into its
        // neighbour. That is the case the anchor has to decline.
        action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        h.stoat.pending_hover = Some(HoverPopup::new(
            vec![vec![("w".repeat(70), Style::default())]],
            0,
            editor_id,
        ));
        h.snapshot();

        assert_eq!(
            h.stoat.pending_hover.as_ref().expect("popup").anchor,
            None,
            "a popup past the pane's region stamps no anchor"
        );

        emit_smooth_scroll(&mut h.stoat);
        let batch = drain_apc(&mut rx);
        assert!(
            !batch
                .iter()
                .any(|cmd| matches!(cmd, Command::PoolAnchor(_))),
            "and nothing tells the terminal to ride it, got {batch:?}"
        );
    }

    #[test]
    fn a_gliding_pane_freezes_the_hover_region_at_the_stamped_rect() {
        use stoatty_protocol::command::Command;

        let (mut h, mut rx, editor_id) = hover_harness();
        let stamped = h.stoat.pending_hover.as_ref().expect("popup").inner;

        // Arm a glide and jump the scroll target past where the popup was laid
        // out. Re-laying against that target is exactly what the freeze avoids.
        {
            let editor = h
                .stoat
                .active_workspace_mut()
                .editors
                .get_mut(editor_id)
                .expect("editor");
            editor.scroll_glide = ScrollGlide::Wheel;
            editor.scroll_row = 2;
        }

        emit_smooth_scroll(&mut h.stoat);
        let batch = drain_apc(&mut rx);

        let region = batch
            .iter()
            .find_map(|cmd| match cmd {
                Command::PoolRegion(r) if r.pool == crate::smooth_scroll::non_pane_pool::HOVER => {
                    Some(*r)
                },
                _ => None,
            })
            .expect("the hover pool stays declared through the glide");

        assert_eq!(
            (region.top, region.left, region.width, region.height),
            (stamped.y, stamped.x, stamped.width, stamped.height),
            "the region holds the rect the live frame stamped"
        );
        assert!(
            !batch.iter().any(|cmd| matches!(
                cmd,
                Command::PoolDrop(d) if d.pool == crate::smooth_scroll::non_pane_pool::HOVER
            )),
            "and the pool stays in the active set, got {batch:?}"
        );
    }

    #[test]
    fn apc_scene_emits_nothing_for_a_plain_editor_frame() {
        let mut h = Stoat::test();
        // The default theme resolves status colors to RGB, which drives the
        // status bar into the scene as sub-cell components. A theme without RGB
        // status colors keeps the status bar in cells, so with line numbers off
        // the frame stays genuinely widget-free.
        h.stoat.theme = Arc::new(rgb_review_theme());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        // Line numbers off so the paint carries no off-grid gutter, and the
        // minimap off so no strip declare rides the scene. Both keep the frame
        // genuinely widget-free.
        h.stoat.settings.editor_line_numbers = Some(LineNumbers::Off);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Off);

        let root = PathBuf::from("/scene");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let mut buf = Buffer::empty(h.stoat.size());
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        assert!(
            drain_apc(&mut rx).is_empty(),
            "a widget-free paint appends nothing, so the scene flush stays silent"
        );
    }

    #[test]
    fn review_gutter_emits_sub_cell_components_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = Arc::new(rgb_review_theme());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        h.open_review_from_texts(&[("a.rs", "fn a() {}\n", "fn a_renamed() {}\n")]);

        let mut buf = Buffer::empty(h.stoat.size());
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|c| matches!(c, Command::TextRun(_))),
            "line numbers emit as sub-cell text runs, got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Command::Bar(_))),
            "status marks and the separator emit as sub-cell bars, got {cmds:?}"
        );
    }

    #[test]
    fn status_bar_emits_sub_cell_components_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        // Line numbers off so the only off-grid components are the status bar's.
        h.stoat.settings.editor_line_numbers = Some(LineNumbers::Off);

        let root = PathBuf::from("/status");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let mut buf = Buffer::empty(h.stoat.size());
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::TextRun(t) if t.col == 0 && t.bg.is_none())),
            "the mode segment emits as a box-less text run at col 0, got {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::Bar(b) if b.height == 16)),
            "the mode segment background emits as a full-row bar, got {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::Bar(b) if b.height == 1)),
            "the status hairline emits as a one-sixteenth bar, got {cmds:?}"
        );
    }

    #[test]
    fn overlay_status_bar_emits_sub_cell_components_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        // A rebase overlay routes its status row through render_overlay_status.
        h.stoat.active_workspace_mut().rebase = Some(crate::rebase::RebaseState::new(
            PathBuf::from("/overlay"),
            "onto".into(),
            vec![],
        ));

        let mut buf = Buffer::empty(h.stoat.size());
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::TextRun(t) if t.col == 0)),
            "the overlay status row emits a text run at col 0, got {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::Bar(b) if b.height == 1)),
            "the overlay status hairline emits as a one-sixteenth bar, got {cmds:?}"
        );
    }

    #[test]
    fn diagnostic_gutter_emits_sub_cell_bars_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = Arc::new(rgb_diagnostic_theme());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/diag-rich");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        h.seed_diagnostics(
            path,
            vec![lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 1,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                ..Default::default()
            }],
        );

        let mut buf = Buffer::empty(h.stoat.size());
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|c| matches!(c, Command::Bar(_))),
            "a severity mark emits a sub-cell bar, got {cmds:?}"
        );
    }

    #[test]
    fn diagnostic_popover_emits_a_popover_frame_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = Arc::new(rgb_diagnostic_theme());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/diag-popover");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"let x = 1;\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        // A span covering the start, so the cursor at offset zero sits inside it.
        h.seed_diagnostics(
            path,
            vec![lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                message: "unexpected token".to_string(),
                ..Default::default()
            }],
        );

        let mut buf = Buffer::empty(h.stoat.size());
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|c| matches!(c, Command::Popover(_))),
            "a diagnostic under the cursor emits a popover frame, got {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::Icon(icon) if icon.offset == [3, 6])),
            "the severity icon carries the popover offset so it sits inside the card, got {cmds:?}"
        );
    }

    #[test]
    fn pane_display_mode_renders_a_bold_digit_badge_per_pane() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        h.type_keys("space a s");
        assert_eq!(h.stoat.active_workspace().panes.pane_count(), 2);

        h.type_keys("space a e");
        let mut buf = Buffer::empty(h.stoat.size());
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let cmds = drain_apc(&mut rx);
        let popovers: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                Command::Popover(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(popovers.len(), 2, "each pane gets a badge, got {cmds:?}");
        assert!(popovers.iter().all(|p| p.bold), "badges shape bold");
        let digits: Vec<&str> = popovers
            .iter()
            .map(|p| buf[(p.left + 1, p.top + 1)].symbol())
            .collect();
        assert!(
            digits.contains(&"1") && digits.contains(&"2"),
            "the pane centers carry the digits 1 and 2, got {digits:?}"
        );

        let hint_box_shown = (buf.area.y..buf.area.y + buf.area.height).any(|y| {
            let row: String = (buf.area.x..buf.area.x + buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect();
            row.contains(" space_pane_display ")
        });
        assert!(
            !hint_box_shown,
            "the chord suppresses the keybinding-hints box, leaving only the badges"
        );

        // Selecting a pane focuses it and returns to normal, so the next frame
        // draws no badges.
        h.type_keys("2");
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);
        assert!(
            !drain_apc(&mut rx)
                .iter()
                .any(|c| matches!(c, Command::Popover(_))),
            "selecting a pane clears the badges"
        );

        // Escape from the chord mode also clears the badges.
        h.type_keys("space a e");
        h.type_keys("escape");
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);
        assert!(
            !drain_apc(&mut rx)
                .iter()
                .any(|c| matches!(c, Command::Popover(_))),
            "escape clears the badges"
        );
    }

    #[test]
    fn diagnostic_popover_dodges_a_cursor_under_the_below_placement() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = Arc::new(rgb_diagnostic_theme());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/diag-popover-dodge");
        let path = root.join("a.txt");
        h.fake_fs()
            .insert_file(&path, b"aaaaa\nbbbbb\nccccc\nddddd\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        // A multi-line span keeps the cursor inside the diagnostic after it moves
        // down, and a multi-line message makes the below-anchor popover tall
        // enough to sit over the row beneath the diagnostic's start.
        h.seed_diagnostics(
            path,
            vec![lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 3,
                        character: 0,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                message: "line one\nline two\nline three".to_string(),
                ..Default::default()
            }],
        );

        // Drop the cursor onto the row the below-anchor popover would occupy.
        action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveDown);

        let mut buf = Buffer::empty(h.stoat.size());
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let (cx, cy) = h
            .stoat
            .primary_cursor_screen_pos()
            .expect("primary cursor on screen");
        let cmds = drain_apc(&mut rx);
        let popover = cmds
            .iter()
            .find_map(|c| match c {
                Command::Popover(p) => Some(p),
                _ => None,
            })
            .expect("a diagnostic popover frame");

        let covers_cursor = cx >= popover.left
            && cx < popover.left + popover.width
            && cy >= popover.top
            && cy < popover.top + popover.height;
        assert!(
            !covers_cursor,
            "popover rect {popover:?} must not cover the cursor cell {:?}",
            (cx, cy)
        );
    }

    #[test]
    fn help_modal_emits_a_panel_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
        h.stoat.theme = Arc::new(rgb_modal_theme());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
        h.settle();

        let size = h.stoat.size();
        let mut buf = Buffer::empty(size);
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let modal = help_layout(&h).modal;
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                Command::Panel(p)
                    if p.top == modal.y
                        && p.left == modal.x
                        && p.width == modal.width
                        && p.height == modal.height
            )),
            "the help modal emits a panel at its layout rect, got {cmds:?}"
        );
    }

    #[test]
    fn help_modal_emits_a_panel_under_the_default_theme() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
        h.settle();

        let size = h.stoat.size();
        let mut buf = Buffer::empty(size);
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let modal = help_layout(&h).modal;
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                Command::Panel(p)
                    if p.top == modal.y
                        && p.left == modal.x
                        && p.width == modal.width
                        && p.height == modal.height
            )),
            "the shipped default theme resolves named colors to RGB, so the \
             help modal takes the rich arm and emits a panel, got {cmds:?}"
        );
    }

    #[test]
    fn help_separator_emits_a_hairline_bar_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
        h.stoat.theme = Arc::new(rgb_modal_theme());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
        h.settle();

        let size = h.stoat.size();
        let mut buf = Buffer::empty(size);
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let list = help_layout(&h).list;
        let sep_x = (list.x + list.width) as i16 * 16 + 8;
        let sep_y = list.y as i16 * 16;
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                Command::Bar(b)
                    if b.x == sep_x
                        && b.y == sep_y
                        && b.width == 1
                        && b.height == list.height * 16
            )),
            "the help list/detail separator emits a hairline bar, got {cmds:?}"
        );
    }

    #[test]
    fn hints_overlay_emits_scaled_text_runs_inside_stoatty() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = Arc::new(rgb_modal_theme());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenHelp);
        h.settle();

        let mut buf = Buffer::empty(h.stoat.size());
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::TextRun(t) if t.scale == 218)),
            "the hints overlay emits 0.85x hint-row text runs, got {cmds:?}"
        );
    }

    #[test]
    fn the_hints_box_anchors_to_the_bottom_right_corner() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = Arc::new(rgb_modal_theme());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.type_keys("space");

        let size = h.stoat.size();
        let mut buf = Buffer::empty(size);
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let panel = drain_apc(&mut rx)
            .into_iter()
            .find_map(|c| match c {
                Command::Panel(p) => Some(p),
                _ => None,
            })
            .expect("the standing hints box emits a panel");
        assert_eq!(
            panel.top + panel.height,
            size.height - 1,
            "the hints box's bottom edge sits just above the reserved status row"
        );
        assert_eq!(
            panel.left + panel.width,
            size.width,
            "the hints box's right edge lands on the window's last column"
        );
    }

    /// The box is declared after every modal, so it lands over the commit
    /// picker's pooled list and preview. Layered with the grid it would be
    /// painted over by their composites for the length of every glide, which is
    /// what made it blink out mid-scroll.
    #[test]
    fn the_hints_box_panel_floats_above_pooled_surfaces() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = Arc::new(rgb_modal_theme());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.type_keys("space");

        let mut buf = Buffer::empty(h.stoat.size());
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let flags: Vec<bool> = drain_apc(&mut rx)
            .into_iter()
            .filter_map(|c| match c {
                Command::Panel(p) => Some(p.above_pools),
                _ => None,
            })
            .collect();
        assert_eq!(
            flags,
            [true],
            "the standing hints box is the frame's one panel, flagged above pools"
        );
    }

    #[test]
    fn hover_body_emits_scaled_text_runs_inside_stoatty() {
        use lsp_types::{HoverProviderCapability, ServerCapabilities};
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.fake_lsp().set_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..Default::default()
        });

        let root = PathBuf::from("/hover-apc");
        let path = root.join("main.rs");
        h.fake_fs().insert_file(&path, b"fn foo() {}\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "fn foo() -> u32");
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        let mut buf = Buffer::empty(h.stoat.size());
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::TextRun(t) if t.scale == 218)),
            "the hover body emits 0.85x scaled text runs under stoatty, got {cmds:?}"
        );
    }

    #[test]
    fn pane_divider_emits_a_hairline_bar_inside_stoatty() {
        use crate::pane::DividerOrientation;
        use stoatty_protocol::command::{BarCommand, Command};

        let mut h = Stoat::test();
        h.stoat.theme = Arc::new(rgb_border_theme());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);

        let size = h.stoat.size();
        let mut buf = Buffer::empty(size);
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let dividers = h.stoat.active_workspace().panes.dividers();
        let d = dividers
            .iter()
            .find(|d| matches!(d.orientation, DividerOrientation::Vertical))
            .expect("the split has a vertical divider");
        let end_y = d.y.saturating_add(d.len).min(size.height);
        let expected = BarCommand {
            x: d.x as i16 * 16 + 8,
            y: d.y as i16 * 16,
            width: 1,
            height: (end_y - d.y) * 16,
            color: if d.touches_focus {
                [0xaa, 0xbb, 0xcc]
            } else {
                [0x55, 0x66, 0x77]
            },
        };
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.contains(&Command::Bar(expected)),
            "the split divider emits a hairline bar in the border color, got {cmds:?}"
        );
    }

    #[test]
    fn completion_popup_emits_a_panel_inside_stoatty() {
        use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        h.stoat.theme = Arc::new(rgb_modal_theme());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/complete");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.type_keys("i");

        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![CompletionItem {
                label: "println".into(),
                source: CompletionSource::Lsp,
                kind: None,
                detail: None,
                replace_range: crate::completion::unused_replace_range(),
                insert_text: "println".into(),
                is_snippet: false,
                documentation: None,
                lsp_item: None,
                server: None,
            }],
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..0,
            prefix: String::new(),
            incomplete: Vec::new(),
        });

        let mut buf = Buffer::empty(h.stoat.size());
        app::paint_frame(&mut h.stoat, &mut buf);
        emit_apc_scene(&mut h.stoat);

        let popup_area = crate::render::completion::completion_popup_layout(&mut h.stoat)
            .expect("completion popup lays out")
            .1
            .popup_area;
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                Command::Panel(p)
                    if p.top == popup_area.y
                        && p.left == popup_area.x
                        && p.width == popup_area.width
                        && p.height == popup_area.height
            )),
            "the completion popup emits a panel at its layout rect, got {cmds:?}"
        );
    }

    #[test]
    fn editor_pool_pages_fill_asynchronously() {
        use stoatty_protocol::command::{Command, FillCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/async-pool");
        let path = root.join("a.txt");
        let body = (0..150).map(|i| format!("line {i}\n")).collect::<String>();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        emit_smooth_scroll(&mut h.stoat);

        // The first batch carries the pool geometry and scroll, but no editor fill:
        // plain editor pages are rendered off the run loop, not inline.
        let first = rx.try_recv().expect("region/scroll batch");
        let first_cmds = command::decode_stream(&first);
        assert!(
            first_cmds
                .iter()
                .any(|c| matches!(c, Command::PoolRegion(_))),
            "first batch declares the pool, got {first_cmds:?}"
        );
        assert!(
            !first_cmds.iter().any(|c| matches!(c, Command::Fill(_))),
            "first batch carries no synchronous editor fill, got {first_cmds:?}"
        );

        // The blocking renders run inline under the test scheduler, so their fills
        // arrive as later batches on the same channel. The initial visible page is 0,
        // whose buffered window is pages 0..5.
        let mut filled = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            for cmd in command::decode_stream(&batch) {
                if let Command::Fill(FillCommand { index, .. }) = cmd {
                    filled.push(index);
                }
            }
        }
        filled.sort_unstable();
        assert_eq!(
            filled,
            vec![0, 1, 2, 3, 4],
            "the initial window's pages fill asynchronously, got {filled:?}"
        );
    }

    #[test]
    fn detached_editor_ships_a_window_bound_pool() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.window_ipc_connected = true;

        let root = PathBuf::from("/detach-emit");
        let path = root.join("a.txt");
        let body = (0..150).map(|i| format!("line {i}\n")).collect::<String>();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);
        h.type_action("SplitRight()");
        h.settle();
        h.type_action("DetachPane()");
        while rx.try_recv().is_ok() {}

        emit_smooth_scroll(&mut h.stoat);
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::PoolRegion(r) if r.window != 0)),
            "the detached editor declares a window-bound pool, got {cmds:?}"
        );
    }

    #[test]
    fn detached_focus_ships_a_pool_cursor_that_goes_quiet_when_idle() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.window_ipc_connected = true;

        let root = PathBuf::from("/detach-cursor");
        let path = root.join("a.txt");
        let body = (0..30).map(|i| format!("line {i}\n")).collect::<String>();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);
        h.type_action("SplitRight()");
        h.settle();
        h.type_action("DetachPane()");
        while rx.try_recv().is_ok() {}

        emit_smooth_scroll(&mut h.stoat);
        assert!(
            drain_apc(&mut rx)
                .iter()
                .any(|c| matches!(c, Command::PoolCursor(_))),
            "the detached focus ships a pool cursor"
        );

        emit_smooth_scroll(&mut h.stoat);
        assert!(
            !drain_apc(&mut rx)
                .iter()
                .any(|c| matches!(c, Command::PoolCursor(_))),
            "an unchanged frame ships no pool cursor"
        );
    }

    /// Acting on an unchanged version now skips `emit_into` outright, and with
    /// it the region declaration that used to go out regardless. So the region
    /// has to be part of what the version is computed from.
    ///
    /// Only the row count changes here. The segments are laid out against the
    /// row's width, so they come back identical and the region is the only thing
    /// that moved.
    #[test]
    fn a_moved_status_row_redeclares_its_region_though_its_segments_match() {
        use stoatty_protocol::command::Command;

        let status_base = crate::smooth_scroll::non_pane_pool::WINDOW_STATUS;
        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.window_ipc_connected = true;

        let root = PathBuf::from("/status-move");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"hello\nworld\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);
        h.type_action("SplitRight()");
        h.settle();
        h.type_action("DetachPane()");

        let panes = &h.stoat.active_workspace().panes;
        let window = match &panes.pane(panes.focus()).placement {
            Placement::Window(window) => *window,
            other => panic!("the pane detaches into a window, got {other:?}"),
        };
        app::deliver_window_event(
            &mut h.stoat,
            WindowIpcEvent::Resized {
                window,
                cols: 80,
                rows: 24,
            },
        );
        emit_smooth_scroll(&mut h.stoat);
        while rx.try_recv().is_ok() {}

        emit_smooth_scroll(&mut h.stoat);
        assert!(
            !drain_apc(&mut rx)
                .iter()
                .any(|c| matches!(c, Command::PoolRegion(r) if r.pool >= status_base)),
            "an unchanged status row re-declares nothing"
        );

        app::deliver_window_event(
            &mut h.stoat,
            WindowIpcEvent::Resized {
                window,
                cols: 80,
                rows: 18,
            },
        );
        emit_smooth_scroll(&mut h.stoat);
        let moved = drain_apc(&mut rx);
        assert!(
            moved
                .iter()
                .any(|c| matches!(c, Command::PoolRegion(r) if r.pool >= status_base)),
            "a shorter window moves the status row up, got {moved:?}"
        );
    }

    #[test]
    fn detached_pane_ships_a_window_status_row_that_goes_quiet_when_idle() {
        use stoatty_protocol::command::Command;

        let status_base = crate::smooth_scroll::non_pane_pool::WINDOW_STATUS;
        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.window_ipc_connected = true;

        let root = PathBuf::from("/detach-status");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"hello\nworld\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);
        h.type_action("SplitRight()");
        h.settle();
        h.type_action("DetachPane()");
        while rx.try_recv().is_ok() {}

        emit_smooth_scroll(&mut h.stoat);
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter().any(
                |c| matches!(c, Command::PoolRegion(r) if r.pool >= status_base && r.window != 0)
            ),
            "the detached pane declares a window-bound status pool, got {cmds:?}"
        );

        emit_smooth_scroll(&mut h.stoat);
        assert!(
            !drain_apc(&mut rx)
                .iter()
                .any(|c| matches!(c, Command::PoolRegion(r) if r.pool >= status_base)),
            "an unchanged status bar re-declares nothing"
        );
    }

    #[test]
    fn detached_terminal_ships_a_content_pool_that_repaints_then_goes_quiet() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.window_ipc_connected = true;
        h.resize(80, 24);
        h.type_action("SplitRight()");
        h.settle();

        // Point the focused split pane at a terminal without leaving normal
        // mode, so DetachPane rides its keybinding as a user would.
        let session: Arc<dyn crate::host::TerminalSession> =
            Arc::new(crate::host::FakeTerminalSession::new());
        let (focused, term_id, index) = {
            let ws = h.stoat.active_workspace_mut();
            let focused = ws.panes.focus();
            let term_id = ws.terms.insert(TermSession::new(
                crate::term_screen::TermScreen::new(24, 80),
                session,
            ));
            ws.panes.pane_mut(focused).view = View::Terminal(term_id);
            (focused, term_id, ws.panes.pane(focused).index)
        };

        h.type_action("DetachPane()");
        assert!(
            matches!(
                h.stoat.active_workspace().panes.pane(focused).placement,
                Placement::Window(_)
            ),
            "a terminal pane detaches into its own window"
        );
        while rx.try_recv().is_ok() {}

        emit_smooth_scroll(&mut h.stoat);
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::PoolRegion(r) if r.pool == index && r.window != 0)),
            "the detached terminal declares a window-bound content pool, got {cmds:?}"
        );

        emit_smooth_scroll(&mut h.stoat);
        let idle = drain_apc(&mut rx);
        assert!(
            !idle
                .iter()
                .any(|c| matches!(c, Command::PoolRegion(r) if r.pool == index)),
            "an idle terminal re-declares no content pool, got {idle:?}"
        );

        h.stoat
            .active_workspace_mut()
            .terms
            .get_mut(term_id)
            .expect("terminal session")
            .term
            .feed(b"detached output");
        emit_smooth_scroll(&mut h.stoat);
        let after = drain_apc(&mut rx);
        assert!(
            after
                .iter()
                .any(|c| matches!(c, Command::Fill(f) if f.pool == index)),
            "terminal output repaints the content pool, got {after:?}"
        );
    }

    #[test]
    fn focus_pane_by_number_reaches_and_raises_a_detached_window() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.window_ipc_connected = true;
        h.resize(80, 24);
        h.type_action("SplitRight()");
        h.settle();
        h.type_action("DetachPane()");
        h.settle();

        let detached = h.stoat.active_workspace().panes.windowed_panes()[0].0;
        action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusPane { index: 1 });
        while rx.try_recv().is_ok() {}

        action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusPane { index: 2 });
        assert_eq!(
            h.stoat.active_workspace().panes.focus(),
            detached,
            "FocusPane reaches the detached pane at selectable position 2"
        );
        assert!(
            drain_apc(&mut rx)
                .iter()
                .any(|c| matches!(c, Command::WindowFocus(_))),
            "focusing a detached pane raises its OS window"
        );
    }

    #[test]
    fn detached_pane_status_badge_continues_the_split_count() {
        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.window_ipc_connected = true;
        h.resize(80, 24);
        h.type_action("SplitRight()");
        h.settle();
        h.type_action("DetachPane()");
        h.settle();

        h.stoat.set_focused_mode("space_pane_display".to_string());
        while rx.try_recv().is_ok() {}
        emit_smooth_scroll(&mut h.stoat);

        let mut bytes = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            bytes.extend(chunk);
        }
        assert!(
            bytes.windows(3).any(|w| w == b"[2]"),
            "a detached pane's status badge continues the split count to 2"
        );
    }

    #[test]
    fn review_pane_is_pooled_for_smooth_scroll() {
        use crate::test_harness::{TestHarness, REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER};
        use stoatty_protocol::command::Command;

        let mut h = TestHarness::with_size(80, 24);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        h.open_review_from_texts(&[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        emit_smooth_scroll(&mut h.stoat);

        let bytes = rx.try_recv().expect("the review pane emits an APC batch");
        let cmds = command::decode_stream(&bytes);
        assert!(
            cmds.iter().any(|cmd| matches!(cmd, Command::PoolRegion(_))),
            "a review split pane declares a smooth-scroll pool, got {cmds:?}"
        );
    }

    #[test]
    fn file_finder_list_is_pooled_and_retired() {
        use stoat_action::OpenFileFinder;
        use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/finder");
        for name in ["a.rs", "b.rs", "c.rs"] {
            h.fake_fs().insert_file(root.join(name), b"x\n");
        }
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        emit_smooth_scroll(&mut h.stoat);
        let list = finder_layout(&h).list;
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::FINDER,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
            window: 0,
        };
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
            "the finder list declares a pool at its list rect"
        );

        h.stoat.file_finder = None;
        emit_smooth_scroll(&mut h.stoat);
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::FINDER,
            })),
            "closing the finder retires its pool"
        );
    }

    /// The pool paints rows the live grid paints anyway, so a keystroke that
    /// re-filters the list has nothing worth filling until the target moves.
    ///
    /// Both halves are load-bearing. A pool that stopped refilling altogether
    /// would satisfy the first assertion, so the second moves the selection past
    /// the visible page and requires the deferred window back.
    #[test]
    fn typing_in_the_finder_defers_its_pool_refill_until_the_list_scrolls() {
        use stoat_action::OpenFileFinder;
        use stoatty_protocol::command::Command;

        fn finder_fills(cmds: &[Command]) -> Vec<u64> {
            cmds.iter()
                .filter_map(|cmd| match cmd {
                    Command::Fill(fill)
                        if fill.pool == crate::smooth_scroll::non_pane_pool::FINDER =>
                    {
                        Some(fill.index)
                    },
                    _ => None,
                })
                .collect()
        }

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/holdfinder");
        for i in 0..200 {
            h.fake_fs()
                .insert_file(root.join(format!("a{i}.rs")), b"x\n");
        }
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        // The first display prefills the whole window, which is the state a
        // keystroke arrives into.
        emit_smooth_scroll(&mut h.stoat);
        let _ = drain_apc(&mut rx);

        h.type_text("a");
        h.settle();
        emit_smooth_scroll(&mut h.stoat);
        let typed = finder_fills(&drain_apc(&mut rx));
        assert!(
            typed.is_empty(),
            "a keystroke into a resting finder fills no page, got {typed:?}"
        );

        // Selecting past the visible page moves the scroll target, which is
        // where the deferred content change applies.
        let list_height = finder_layout(&h).list.height as usize;
        h.stoat
            .file_finder
            .as_mut()
            .expect("the finder is open")
            .active_core()
            .picklist
            .selected = list_height;
        emit_smooth_scroll(&mut h.stoat);
        assert!(
            !finder_fills(&drain_apc(&mut rx)).is_empty(),
            "the deferred window fills once the list scrolls"
        );
    }

    #[test]
    fn finder_pool_region_spans_the_full_window_over_the_band() {
        use stoat_action::OpenFileFinder;
        use stoatty_protocol::command::{Command, PoolRegionCommand};

        let mut h = Stoat::test();
        h.resize(120, 24);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/bandfinder");
        for name in ["a.rs", "b.rs", "c.rs"] {
            h.fake_fs().insert_file(root.join(name), b"x\n");
        }
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);

        // Paint one frame so the single-minimap band is stamped. Single is the
        // default mode and the test terminal is wide enough to reserve the strip.
        h.snapshot();
        assert_ne!(
            h.stoat.layout_size(),
            h.stoat.size(),
            "the single-minimap band must be reserved so the test proves the modal ignores it"
        );
        let _ = drain_apc(&mut rx);

        emit_smooth_scroll(&mut h.stoat);
        let list = finder_layout(&h).list;
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::FINDER,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
            window: 0,
        };
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
            "the finder pool spans the full window even with the band reserved, so the modal takes precedence over the strip"
        );
    }

    #[test]
    fn palette_list_is_pooled_and_retired() {
        use stoat_action::OpenCommandPalette;
        use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        action_handlers::dispatch(&mut h.stoat, &OpenCommandPalette);
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        emit_smooth_scroll(&mut h.stoat);
        let (rows, zoom) = palette_sizing(&h);
        let list =
            crate::render::command_palette::palette_filter_layout(h.stoat.size(), rows, zoom)
                .expect("the palette fits the test terminal")
                .list;
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::PALETTE,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
            window: 0,
        };
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
            "the palette list declares a pool at its list rect"
        );

        h.stoat.command_palette = None;
        emit_smooth_scroll(&mut h.stoat);
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::PALETTE,
            })),
            "closing the palette retires its pool"
        );
    }

    #[test]
    fn palette_arg_list_is_pooled_and_retired() {
        use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/argpool");
        for name in ["a.rs", "b.rs", "c.rs"] {
            h.fake_fs().insert_file(root.join(name), b"x\n");
        }
        h.stoat.active_workspace_mut().git_root = root;
        // Typing `:o ` opens the palette and installs the Files arg picker. The
        // snapshot drives drive_background so the picker is live before emit.
        h.type_text(":o ");
        h.snapshot();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);
        let _ = drain_apc(&mut rx);

        emit_smooth_scroll(&mut h.stoat);
        let (rows, zoom) = palette_sizing(&h);
        let list =
            crate::render::command_palette::palette_arg_list_rect(h.stoat.size(), rows, zoom)
                .expect("the arg picker fits the test terminal");
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::PALETTE,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
            window: 0,
        };
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
            "the arg-picker list declares a pool at its list rect"
        );

        h.stoat.command_palette = None;
        emit_smooth_scroll(&mut h.stoat);
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::PALETTE,
            })),
            "closing the palette retires its pool"
        );
    }

    #[test]
    fn palette_filter_to_arg_flip_repools() {
        use stoat_action::OpenCommandPalette;
        use stoatty_protocol::command::{Command, PoolRegionCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/argflip");
        for name in ["a.rs", "b.rs"] {
            h.fake_fs().insert_file(root.join(name), b"x\n");
        }
        h.stoat.active_workspace_mut().git_root = root;

        // Filter mode holds the PALETTE pool through the command list.
        action_handlers::dispatch(&mut h.stoat, &OpenCommandPalette);
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);
        emit_smooth_scroll(&mut h.stoat);
        let _ = drain_apc(&mut rx);

        // Flipping to arg mode re-declares the same pool at the arg-list rect.
        h.type_text("o ");
        h.snapshot();
        h.stoat.active_workspace_mut().layout(size);
        emit_smooth_scroll(&mut h.stoat);

        let (rows, zoom) = palette_sizing(&h);
        let arg_list =
            crate::render::command_palette::palette_arg_list_rect(h.stoat.size(), rows, zoom)
                .expect("the arg picker fits the test terminal");
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::PALETTE,
            top: arg_list.y,
            left: arg_list.x,
            width: arg_list.width,
            height: arg_list.height,
            window: 0,
        };
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
            "flipping filter to arg mode re-declares the pool at the arg-list rect"
        );
    }

    #[test]
    fn completion_popup_is_pooled_and_retired() {
        use crate::completion::{CompletionItem, CompletionPopup, CompletionSource};
        use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        let item = |label: &str| CompletionItem {
            label: label.into(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            replace_range: crate::completion::unused_replace_range(),
            insert_text: label.into(),
            is_snippet: false,
            documentation: None,
            lsp_item: None,
            server: None,
        };
        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![item("alpha"), item("beta"), item("gamma")],
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..0,
            prefix: String::new(),
            incomplete: Vec::new(),
        });

        emit_smooth_scroll(&mut h.stoat);
        let (_, layout) = crate::render::completion::completion_popup_layout(&mut h.stoat)
            .expect("the popup anchors in the test terminal");
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::COMPLETION,
            top: layout.inner.y,
            left: layout.inner.x,
            width: layout.inner.width,
            height: layout.inner.height,
            window: 0,
        };
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolRegion(expected)),
            "the completion popup declares a pool at its inner rect"
        );

        h.stoat.pending_completion = None;
        emit_smooth_scroll(&mut h.stoat);
        assert!(
            drain_apc(&mut rx).contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::COMPLETION,
            })),
            "closing the popup retires its pool"
        );
    }

    #[test]
    fn help_list_and_detail_are_pooled_and_retired() {
        use stoat_action::OpenHelp;
        use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        action_handlers::dispatch(&mut h.stoat, &OpenHelp);
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        emit_smooth_scroll(&mut h.stoat);
        let layout = help_layout(&h);
        let list = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::HELP_LIST,
            top: layout.list.y,
            left: layout.list.x,
            width: layout.list.width,
            height: layout.list.height,
            window: 0,
        };
        let detail = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::HELP_DETAIL,
            top: layout.detail.y,
            left: layout.detail.x,
            width: layout.detail.width,
            height: layout.detail.height,
            window: 0,
        };
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.contains(&Command::PoolRegion(list)),
            "the help list declares a pool at its rect"
        );
        assert!(
            cmds.contains(&Command::PoolRegion(detail)),
            "the help detail declares a pool at its rect"
        );

        h.stoat.help = None;
        emit_smooth_scroll(&mut h.stoat);
        let cmds = drain_apc(&mut rx);
        assert!(
            cmds.contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::HELP_LIST,
            })),
            "closing help retires the list pool"
        );
        assert!(
            cmds.contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::HELP_DETAIL,
            })),
            "closing help retires the detail pool"
        );
    }

    #[test]
    fn commits_list_is_pooled_and_retired() {
        use crate::commit_list::CommitListState;
        use stoatty_protocol::command::{Command, PoolDropCommand, PoolRegionCommand};

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);
        h.stoat.active_workspace_mut().commits = Some(CommitListState::new(PathBuf::from("/work")));
        h.stoat.set_focused_mode("commits".to_string());

        emit_smooth_scroll(&mut h.stoat);
        let focused = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        let list = crate::render::commits::commits_list_rect(focused)
            .expect("the commits list fits the test terminal");
        let expected = PoolRegionCommand {
            pool: crate::smooth_scroll::non_pane_pool::COMMITS,
            top: list.y,
            left: list.x,
            width: list.width,
            height: list.height,
            window: 0,
        };
        let bytes = rx
            .try_recv()
            .expect("the commits overlay emits an APC batch");
        assert!(
            command::decode_stream(&bytes).contains(&Command::PoolRegion(expected)),
            "the commits list declares a pool at its list rect"
        );

        h.stoat.set_focused_mode("normal".to_string());
        h.stoat.active_workspace_mut().commits = None;
        emit_smooth_scroll(&mut h.stoat);
        let bytes = rx.try_recv().expect("leaving commits emits a drop");
        assert!(
            command::decode_stream(&bytes).contains(&Command::PoolDrop(PoolDropCommand {
                pool: crate::smooth_scroll::non_pane_pool::COMMITS,
            })),
            "leaving commits mode retires its pool"
        );
    }

    /// A conflict pane is a `View::Editor`, so without its own fill arm it
    /// routes to the plain-editor page and its pooled fills paint the bare
    /// center scratch buffer, dropping the flanking columns wherever a pool page
    /// covers the region. Pins the routing rather than the page renderer.
    #[test]
    fn a_conflict_pane_fills_with_the_three_column_body() {
        use crate::render::conflict_view::render_conflict_rows;
        use ratatui::layout::Rect;

        let mut h = crate::test_harness::TestHarness::with_size(150, 24);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git()
            .add_repo("/repo")
            .with_fs(h.fake_fs())
            .conflicted_file(
                "f.txt",
                Some("a\nbase\nz\n"),
                Some("a\nOURS\nz\n"),
                Some("a\nTHEIRS\nz\n"),
            );
        action_handlers::dispatch(&mut h.stoat, &Conflict);
        h.settle();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        emit_smooth_scroll(&mut h.stoat);
        // The fills render on blocking workers, so settle before draining.
        h.settle();

        let (_, _, region) = editor_pool_panes(&h.stoat)
            .into_iter()
            .next()
            .expect("the conflict pane declares a pool region");
        let theme = Arc::new(h.stoat.theme.clone());
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let (snapshot, mut state) = {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("center editor");
            (
                editor.display_map.snapshot(),
                editor.conflict_view.clone().expect("conflict view state"),
            )
        };

        let area = Rect::new(0, 0, region.width, region.height);
        let mut expected = crate::smooth_scroll::page_buffer(area, &theme);
        let (doc, derived) = Arc::make_mut(&mut state).derived(&snapshot);
        render_conflict_rows(
            &snapshot,
            doc,
            derived,
            0,
            area,
            fallback,
            &theme,
            &mut expected,
            None,
            None,
        );
        let expected = crate::render::serialize_buffer(&expected);

        let mut fills = Vec::new();
        while let Ok(bytes) = rx.try_recv() {
            fills.push(bytes);
        }
        assert!(
            fills
                .iter()
                .any(|batch| batch.windows(expected.len()).any(|w| w == expected)),
            "a pooled fill carries the live three-column body, not the bare center buffer",
        );
    }

    #[test]
    fn emit_smooth_scroll_pushes_pool_region_then_scroll() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/pool");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        // Lay the panes out so the focused editor has a non-zero rect.
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        emit_smooth_scroll(&mut h.stoat);

        let bytes = rx.try_recv().expect("an APC batch was pushed");
        let cmds = command::decode_stream(&bytes);
        assert!(
            matches!(cmds.first(), Some(Command::PoolRegion(_))),
            "first frame should declare the pool region, got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Command::Scroll(_))),
            "the scroll target eases into the region behind it, got {cmds:?}"
        );
    }

    #[test]
    fn emit_after_edit_reenters_pool_pages() {
        use stoatty_protocol::command::{Command, FillCommand};
        use tokio::sync::mpsc::UnboundedReceiver;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        // A file outside any git root keeps diff_version at 0, so only the
        // display snapshot version can carry an edit into the page content hash.
        let root = PathBuf::from("/pool");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        fn drain_fills(rx: &mut UnboundedReceiver<Vec<u8>>) -> Vec<u64> {
            let mut filled = Vec::new();
            while let Ok(batch) = rx.try_recv() {
                for cmd in command::decode_stream(&batch) {
                    if let Command::Fill(FillCommand { index, .. }) = cmd {
                        filled.push(index);
                    }
                }
            }
            filled
        }

        emit_smooth_scroll(&mut h.stoat);
        assert!(
            !drain_fills(&mut rx).is_empty(),
            "the first emit prefills the pool window"
        );

        // The editor pool refills only while its scroll target moves. A sub-page
        // glide keeps the same page window, so a bare scroll re-enters nothing.
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.scroll_offset = 0.5;
            editor.scroll_glide = ScrollGlide::Page;
        }
        emit_smooth_scroll(&mut h.stoat);
        assert!(
            drain_fills(&mut rx).is_empty(),
            "a same-window scroll with no content change re-enters no pages"
        );

        h.edit_focused(0..0, "x");
        let _ = drain_fills(&mut rx);

        // The edit bumps the snapshot version, so the next moving emit wipes the
        // buffered window and re-enters its pages rather than compositing stale
        // pre-edit text.
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.scroll_offset = 0.9;
            editor.scroll_glide = ScrollGlide::Page;
        }
        emit_smooth_scroll(&mut h.stoat);
        assert!(
            !drain_fills(&mut rx).is_empty(),
            "a scroll after an edit re-enters the pool's pages with fresh text"
        );
    }

    #[test]
    fn emit_smooth_scroll_glide_uses_the_eased_offset() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/pool");
        let path = root.join("a.txt");
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        // Simulate a page glide. scroll_row jumped to a distant row while
        // scroll_offset still lags near the top. The emit must carry the lagging
        // offset (its page is 0), not the target row's page, so the pool eases
        // up to it.
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.scroll_row = 50;
            editor.scroll_offset = 1.0;
            editor.scroll_glide = ScrollGlide::Page;
        }

        emit_smooth_scroll(&mut h.stoat);

        let bytes = rx.try_recv().expect("an APC batch was pushed");
        let cmds = command::decode_stream(&bytes);
        let scroll = cmds
            .iter()
            .find_map(|c| match c {
                Command::Scroll(s) => Some(*s),
                _ => None,
            })
            .expect("a scroll command");
        assert_eq!(
            scroll.page, 0,
            "a glide emits the eased offset's page (1.0 -> page 0), not scroll_row 50's page"
        );
    }

    #[test]
    fn emit_smooth_scroll_anchors_the_cursor_during_a_wheel_glide() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/pool");
        let path = root.join("a.txt");
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        // Move the cursor off the first line so the anchored row is non-trivial,
        // then drop the batches those moves pushed.
        for _ in 0..15 {
            action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveDown);
        }
        while rx.try_recv().is_ok() {}

        // Collect every batch the emit and its async page fills push. An idle
        // pane (no glide) ships no cursor anchor.
        emit_smooth_scroll(&mut h.stoat);
        let mut idle = Vec::new();
        while let Ok(bytes) = rx.try_recv() {
            idle.extend(command::decode_stream(&bytes));
        }
        assert!(
            !idle.iter().any(|c| matches!(c, Command::PoolCursor(_))),
            "an idle emit carries no cursor anchor, got {idle:?}"
        );

        // Arm a wheel glide with a known on-screen cursor cell.
        let expected_row = {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.scroll_glide = ScrollGlide::Wheel;
            editor.cursor_screen_cell = Some((7, 3));
            view::cursor_display_row(editor) as u64
        };
        assert_eq!(expected_row, 15, "the cursor sits on display row 15");

        emit_smooth_scroll(&mut h.stoat);
        let mut cmds = Vec::new();
        while let Ok(bytes) = rx.try_recv() {
            cmds.extend(command::decode_stream(&bytes));
        }
        let anchor = cmds
            .iter()
            .find_map(|c| match c {
                Command::PoolCursor(p) => Some(*p),
                _ => None,
            })
            .expect("a pool_cursor frame while gliding");
        assert_eq!(
            (anchor.row, anchor.col),
            (15, 7),
            "the anchor carries the cursor's display row and recorded column"
        );
    }

    #[test]
    fn a_jump_ships_the_cursor_anchor_at_the_landed_row() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/pool");
        let path = root.join("a.txt");
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);
        while rx.try_recv().is_ok() {}

        // Record line 80 in the jumplist, then strand the view back at the top
        // so returning to that entry is a long jump down.
        let line_80 = body.find("line 80\n").expect("line 80 exists");
        movement::jump_to_offset(&mut h.stoat, line_80);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        movement::jump_to_offset(&mut h.stoat, 0);
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            editor.cursor_screen_cell = Some((4, 2));
            view::ensure_cursor_in_view(editor, 3);
            editor.scroll_glide = ScrollGlide::None;
        }

        action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            assert_eq!(
                editor.scroll_glide,
                ScrollGlide::Page,
                "the jump back arms the glide the anchor emit gates on"
            );
        }

        emit_smooth_scroll(&mut h.stoat);
        let mut cmds = Vec::new();
        while let Ok(bytes) = rx.try_recv() {
            cmds.extend(command::decode_stream(&bytes));
        }
        let anchor = cmds
            .iter()
            .find_map(|c| match c {
                Command::PoolCursor(p) => Some(*p),
                _ => None,
            })
            .expect("a jump arms a glide, so the emit carries a cursor anchor");
        assert_eq!(
            (anchor.row, anchor.col),
            (80, 4),
            "the anchor carries the landed display row, so the cursor rides the content to it"
        );
    }

    #[test]
    fn wheel_glide_defers_the_relative_line_refill_to_settle() {
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/glide");
        let path = root.join("a.txt");
        let body: String = (0..400).map(|i| format!("line {i}\n")).collect();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        let size = h.stoat.size();
        h.stoat.active_workspace_mut().layout(size);

        // Relative numbering is the default. Bake the resting window so the pool
        // holds its pages and pool_current_line records the resting cursor line.
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
        }
        emit_smooth_scroll(&mut h.stoat);
        h.settle();
        let _ = drain_apc(&mut rx);

        // Arm a wheel glide. wheel_scroll advances the target and drags the
        // cursor's buffer line into the scrolloff band, the drag that would churn
        // the relative-number content version every tick without the held line.
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            view::wheel_scroll(editor, true);
        }
        app::tick_animation(&mut h.stoat, 0.016);
        assert!(app::animating(&h.stoat), "the wheel glide is still easing");

        // Mid-glide the held line keeps the content version stable, so the
        // buffered window does not refill. At most one edge page enters.
        emit_smooth_scroll(&mut h.stoat);
        h.settle();
        let mid = drain_apc(&mut rx);
        let mid_fills = mid.iter().filter(|c| matches!(c, Command::Fill(_))).count();
        assert!(
            mid_fills <= 1,
            "a held-line glide refills at most one edge page, got {mid_fills}: {mid:?}"
        );

        // Settling the glide releases the held line. The fresh cursor line bumps
        // the content version once, refilling the whole window to match the grid.
        for _ in 0..1000 {
            if !app::animating(&h.stoat) {
                break;
            }
            app::tick_animation(&mut h.stoat, 0.016);
        }
        emit_smooth_scroll(&mut h.stoat);
        h.settle();
        let settled = drain_apc(&mut rx);
        let settled_fills = settled
            .iter()
            .filter(|c| matches!(c, Command::Fill(_)))
            .count();
        assert!(
            settled_fills > 1,
            "the settle emit refills the whole window once, got {settled_fills}: {settled:?}"
        );
    }

    #[test]
    fn error_popout_emits_scaled_runs_under_stoatty() {
        use crate::render::TEXT_SCALE_COMPACT;
        use lsp_types::MessageType;
        use stoatty_protocol::command::Command;

        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        let msg = "rust-analyzer failed to load the workspace: Cargo.toml is malformed and could not be parsed, so diagnostics are unavailable";
        h.stoat.lsp_message = Some((MessageType::ERROR, msg.to_string()));

        let buf = h.stoat.render();
        emit_apc_scene(&mut h.stoat);

        let mut raw = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            raw.extend(batch);
        }
        let scene = String::from_utf8_lossy(&raw);
        let cmds = command::decode_stream(&raw);

        // A run's text streams between its open and close markers, so it reaches
        // the terminal through the APC scene rather than the cell grid.
        assert!(
            scene.contains("rust-analyzer"),
            "the error head streams into the APC scene"
        );
        assert!(
            scene.contains("diagnostics"),
            "the wrapped tail streams into the APC scene"
        );

        // The popout's bottom line paints directly above the bar at the bar's
        // compact scale.
        let popout_bottom = (buf.area.height - 2) as i16 * 16;
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                Command::TextRun(t) if t.scale == TEXT_SCALE_COMPACT && t.row == popout_bottom
            )),
            "the popout paints as a compact-scale run above the bar"
        );

        // The rich arm keeps the text off the cell grid, unlike the fallback.
        let in_cells = (0..buf.area.height).any(|y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.contains("rust-analyzer")
        });
        assert!(!in_cells, "no grid cell carries the error text");
    }

    #[test]
    fn transient_status_message_keeps_the_rich_status_bar() {
        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);
        h.stoat.set_status("Current working directory is now /w");

        let buf = h.stoat.render();
        emit_apc_scene(&mut h.stoat);

        let mut raw = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            raw.extend(batch);
        }
        let scene = String::from_utf8_lossy(&raw);

        assert!(
            scene.contains("Current working directory"),
            "the status message streams into the APC scene"
        );
        let in_cells = (0..buf.area.height).any(|y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.contains("Current working directory")
        });
        assert!(!in_cells, "no grid cell carries the status message");
    }

    #[test]
    fn quitting_hands_the_terminal_its_own_defaults_back() {
        let mut h = Stoat::test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_apc_tx(tx);

        emit_reset_default_colors(&h.stoat);

        let sent: Vec<u8> = std::iter::from_fn(|| rx.try_recv().ok())
            .flatten()
            .collect();
        assert_eq!(
            sent,
            b"\x1b]110\x1b\\\x1b]111\x1b\\".to_vec(),
            "OSC 110 and 111 undo the defaults the session overrode",
        );
    }
}
