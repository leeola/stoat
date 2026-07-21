pub(crate) mod badges;
pub(crate) mod chrome;
pub(crate) mod code_action;
pub(crate) mod code_search;
pub(crate) mod command_palette;
pub(crate) mod commits;
pub(crate) mod completion;
pub(crate) mod conflict;
pub(crate) mod conflict_view;
pub(crate) mod diagnostics_picker;
pub(crate) mod dock;
pub(crate) mod editor;
pub(crate) mod file_finder;
pub(crate) mod help;
pub(crate) mod hints;
pub(crate) mod hover;
pub(crate) mod jumplist_picker;
pub(crate) mod layout;
pub(crate) mod location_picker;
pub(crate) mod pane;
pub(crate) mod picker;
pub(crate) mod popout;
pub(crate) mod quit_all_confirm;
pub(crate) mod rebase;
pub(crate) mod rename_input;
pub(crate) mod review;
pub(crate) mod reword;
pub(crate) mod run_pane;
pub(crate) mod sanitize;
pub(crate) mod signature_help;
pub(crate) mod symbol_finder;
pub(crate) mod symbol_picker;
pub(crate) mod term_pane;
pub(crate) mod text;
pub(crate) mod undercurl;
pub(crate) mod workspace_picker;

use self::undercurl::UndercurlSpan;
use crate::{
    app::{self, Stoat},
    buffer::BufferId,
    buffer_registry::BufferRegistry,
    editor_state::{EditorId, EditorState},
    keymap_state::{
        action_display_desc, cursor_token, focus_flags, Flags, FocusFlags, StoatKeymapState,
    },
    minimap::MinimapContent,
    pane::{DockVisibility, FocusTarget, View},
    rebase::RebasePause,
    run::{RunId, RunState},
    term_session::{TermId, TermSession},
    workspace::{Workspace, WorkspaceId},
};
use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
use slotmap::SlotMap;
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    path::Path,
};
use stoat_config::{LineNumbers, MinimapMode, WrapMode};
use stoatty_widgets::{minimap::Minimap, popover::Popover, ApcScene};

/// Full-cell text scale under stoatty, in 256ths of a cell, for grid-size modal
/// titles.
pub(crate) const TEXT_SCALE_FULL: u16 = 256;
/// Popup body text scale under stoatty, 0.85x a cell, for hint rows and hover
/// bodies.
pub(crate) const TEXT_SCALE_POPUP: u16 = 218;
/// Compact chrome text scale under stoatty, 0.625x a cell, for line numbers and
/// the status bar.
///
/// Under `cfg(test)` it resolves to a full cell (256) instead: the test
/// compositor draws one glyph per cell, so running compact text at cell scale
/// keeps composited geometry exact against the fallback layout the harness scans
/// and snapshots.
pub(crate) const TEXT_SCALE_COMPACT: u16 = if cfg!(test) { 256 } else { 160 };

/// Strip id for the single-mode minimap, reserved above every pane index (which
/// are small and dense from zero). stoatty keys minimap strips in a namespace
/// separate from scroll pools, so `u32::MAX` never collides.
pub(crate) const SINGLE_MINIMAP_STRIP_ID: u32 = u32::MAX;

pub(crate) struct PaneCtx<'a> {
    pub(crate) editors: &'a mut SlotMap<EditorId, EditorState>,
    pub(crate) buffers: &'a BufferRegistry,
    pub(crate) runs: &'a SlotMap<RunId, RunState>,
    pub(crate) terms: &'a SlotMap<TermId, TermSession>,
}

/// The lookup and colors a pane needs to declare its minimap strip.
///
/// Carried on [`FrameCtx`] so a pane resolves its strip's content-store id (via
/// `(workspace, buffer)`) and the palette and thumb color stoatty paints it in,
/// without the render path reaching back into [`Stoat`].
#[derive(Clone, Copy)]
pub(crate) struct MinimapChrome<'a> {
    /// Active workspace, the first half of the [`Stoat::minimap_content`] key.
    pub(crate) workspace: WorkspaceId,
    /// Content stores this session declared, read for each strip's `content_id`.
    pub(crate) content: &'a HashMap<(WorkspaceId, BufferId), MinimapContent>,
    /// Syntax-scope palette the strip declares and its run summaries index.
    pub(crate) palette: &'a [[u8; 3]],
    /// Viewport-thumb fill color, rgba.
    pub(crate) thumb: [u8; 4],
}

/// Ambient workspace and frame state shared across render functions. Bundled
/// so individual render functions stay under the `clippy::too_many_arguments`
/// threshold; every field is a cheap borrow or `Copy`.
#[derive(Clone, Copy)]
pub(crate) struct FrameCtx<'a> {
    pub(crate) workspace_name: &'a str,
    pub(crate) workspace_root: &'a Path,
    pub(crate) mode: &'a str,
    /// The app screen in the foreground, from [`crate::keymap_state::view_predicate`].
    /// Drives the status-bar screen label. `None` or `Some("file")` for a plain
    /// editor with no screen over it.
    pub(crate) screen: Option<&'static str>,
    pub(crate) theme: &'a crate::theme::Theme,
    /// Mid-typing count prefix waiting on a motion (e.g. `4` between
    /// keypresses on the way to `4j`). The status bar shows it so the
    /// user knows a partial count is in flight; cleared after every
    /// action dispatch.
    pub(crate) pending_count: Option<u32>,
    /// Whether the detailed LSP status popout is open (pinned) above the focused
    /// pane's status bar. Drives whether the multi-row status card paints.
    pub(crate) lsp_status_open: bool,
    /// Every in-flight LSP work-done entry across servers, freshest first, painted
    /// as rows of the detailed status card when it is open.
    pub(crate) lsp_progress_entries: &'a [&'a crate::lsp::progress::LspProgressEntry],
    /// Braille spinner glyph index for the [`lsp_progress`](Self::lsp_progress)
    /// popout, advanced by the frame tick so the spinner animates.
    pub(crate) spinner_phase: u8,
    /// The focused buffer's running language servers as `(short name, busy)`,
    /// painted as compact badges at the focused pane's bar right edge. `busy` is
    /// true while the server has work-done progress in flight, driving the badge's
    /// spinner glyph. Empty for an unfocused pane or a buffer with no named
    /// server.
    pub(crate) lsp_servers: &'a [(String, bool)],
    /// True while a background diff warm runs, driving the focused pane's
    /// transient ` <spinner> diff ` status-bar segment. False when idle.
    pub(crate) diff_warm_busy: bool,
    /// Label of the explicit LSP request still in flight, so the status bar
    /// shows a "lsp: {label}..." segment until the response lands. `None` when no
    /// such request is pending. See [`crate::app::Stoat::lsp_pending_label`].
    pub(crate) lsp_pending: Option<&'static str>,
    /// Freshest `window/showMessage` text. `MessageType::ERROR` paints as a
    /// wrapped popout card above the status bar. Other levels paint in the bar's
    /// right side.
    pub(crate) lsp_message: Option<(lsp_types::MessageType, &'a str)>,
    /// The transient status message ([`Stoat::pending_message`]), already
    /// checked against its TTL deadline. Painted in the focused pane's status
    /// bar just left of the diagnostics badge, styled as an error.
    pub(crate) status_message: Option<&'a str>,
    /// Active labels for an in-progress `GotoWord` jump, keyed by label
    /// string with byte-offset values. Painted by the focused editor's
    /// render path; non-focused panes ignore this field.
    pub(crate) goto_word_labels: Option<&'a std::collections::BTreeMap<String, usize>>,
    /// Per-mode status-line badge overrides resolved from `Settings`.
    /// `mode_segment` consults this before falling back to its hardcoded
    /// badge table; user-defined modes can supply an entry here so the
    /// status line shows something more meaningful than `---`.
    pub(crate) mode_badges: &'a std::collections::BTreeMap<String, String>,
    /// Workspace-wide LSP diagnostic store. The status bar reads the
    /// focused buffer's path and paints a compact severity badge when
    /// any diagnostics are present.
    pub(crate) diagnostics: &'a crate::diagnostics::DiagnosticSet,
    /// The LSP registry, so the editor render path can resolve each server's
    /// negotiated offset encoding on demand.
    ///
    /// Diagnostic positions convert to byte columns through their publishing
    /// server's encoding, so a utf-16 server's undercurl paints on the right
    /// column of a multibyte line. Held as the registry rather than a prebuilt
    /// map so the encodings are resolved only when the diagnostic span cache
    /// rebuilds, not on every frame.
    pub(crate) lsp_registry: &'a crate::lsp::registry::LspRegistry,
    /// Most-recently submitted in-buffer search query. When `Some`,
    /// every editor pane paints visible matches with the
    /// `ui.search.match` style so users see all hits at once.
    pub(crate) search_query: Option<&'a str>,
    /// How document editor panes number the gutter, resolved from
    /// `editor.line_numbers` (default [`LineNumbers::Relative`]).
    /// [`LineNumbers::Off`] keeps the diagnostic-only gutter column.
    pub(crate) line_numbers: LineNumbers,
    /// How document editor panes soft-wrap long lines, resolved from
    /// `editor.wrap` (default [`WrapMode::EditorWidth`]). Applied only to pane
    /// editors. Non-pane inputs and pickers never wrap.
    pub(crate) wrap_mode: WrapMode,
    /// The wrap column [`WrapMode::Bounded`] caps against, resolved from
    /// `editor.wrap_column` (default 80, at least 1). Ignored by the other wrap
    /// modes.
    pub(crate) wrap_column: u32,
    /// Fraction an unfocused pane's colors blend toward the theme background,
    /// resolved from `ui.inactive_dim` (default 0.25, clamped to `0.0..=1.0`).
    /// `0.0` disables dimming. Applied by [`crate::render::pane::render_pane`]
    /// to unfocused panes only.
    pub(crate) inactive_dim: f32,
    /// Whether editor panes reserve their own per-pane right-edge minimap strip.
    /// `true` only in [`MinimapMode::PerPane`] under stoatty. Single mode gates
    /// this off and declares one shared strip over the reserved band instead.
    pub(crate) minimap_enabled: bool,
    /// The lookup and colors a pane needs to declare its minimap strip, `Some`
    /// only when the strip is active (under stoatty with the minimap enabled).
    pub(crate) minimap_chrome: Option<MinimapChrome<'a>>,
    /// The reserved single-minimap band, `Some` only when [`MinimapMode::Single`]
    /// stamped one this frame. It stops one row above the bottom, so a pane whose
    /// status bar sits on that freed row flush against the band reclaims its
    /// width and runs edge to edge.
    pub(crate) minimap_band: Option<Rect>,
    /// Terminal cell the mouse last rested over, or `None` when it has not
    /// moved over a pane. The focused editor resolves the diagnostic under it
    /// to raise a hover popover.
    pub(crate) hover_cell: Option<(u16, u16)>,
    /// The user home directory, for `~`-abbreviating run-pane prompt cwds.
    /// Resolved through [`crate::host::EnvHost`] so tests control it instead of
    /// the paint reading the real environment. `None` when `$HOME` is unset.
    pub(crate) home: Option<&'a Path>,
    /// Latency readout for the status bar, or `None` before any frame has
    /// been painted. Present only under the `perf` feature.
    #[cfg(feature = "perf")]
    pub(crate) perf: Option<PerfSegment>,
}

/// A status-bar latency readout holding the most recent paint time and the
/// p95 input-to-publish latency, both in microseconds.
#[cfg(feature = "perf")]
#[derive(Clone, Copy)]
pub(crate) struct PerfSegment {
    pub(crate) last_paint_us: u64,
    pub(crate) p95_input_us: u64,
}

#[cfg(feature = "perf")]
impl PerfSegment {
    /// Read the headline metrics for the status bar, or `None` until at least
    /// one frame has been painted (so a fresh session shows no readout).
    pub(crate) fn capture(perf: &crate::perf::PerfStats) -> Option<PerfSegment> {
        let paint = perf.paint_stats()?;
        Some(PerfSegment {
            last_paint_us: paint.last / 1_000,
            p95_input_us: perf.input_to_publish_stats().map_or(0, |s| s.p95 / 1_000),
        })
    }
}

/// Format a [`PerfSegment`] for the status bar, padded like the other
/// segments so neighbors stay separated.
#[cfg(feature = "perf")]
pub(crate) fn perf_label(seg: PerfSegment) -> String {
    format!(
        " paint {}us in-p95 {}us ",
        seg.last_paint_us, seg.p95_input_us
    )
}

pub(crate) const PRIMARY_MODES: &[&str] = &["normal", "insert"];

/// Reserve the bottom row for the pane status bar so the which-key hint box,
/// bottom-anchored over the full-window right edge, never paints over it.
pub(crate) fn hints_overlay_area(size: Rect) -> Rect {
    Rect {
        x: size.x,
        y: size.y,
        width: size.width,
        height: size.height.saturating_sub(1),
    }
}

/// True while any centered modal owns the screen's right edge.
///
/// The single-minimap strip and every modal draw in the same GPU passes with
/// the strip on top, so a modal cannot paint over the strip. Instead the strip
/// is undeclared on frames where a modal is open, and the modal lays out on the
/// full window rather than yielding the band. These are the ten mutually
/// exclusive overlays of the frame's modal chain.
fn modal_overlay_open(stoat: &Stoat) -> bool {
    stoat.modal_run.is_some()
        || stoat.help.is_some()
        || stoat.file_finder.is_some()
        || stoat.symbol_finder.is_some()
        || stoat.command_palette.is_some()
        || stoat.workspace_picker.is_some()
        || stoat.quit_all_confirm.is_some()
        || stoat.jumplist_picker.is_some()
        || stoat.diagnostics_picker.is_some()
        || stoat.location_picker.is_some()
        || stoat.code_search.is_some()
}

/// Paint one full frame of the TUI into `buf`. Called once per [`Stoat::render`]
/// tick after the parse pipeline and commits pump have run.
///
/// Retires an expired [`Stoat::pending_message`] up front, then hands the live
/// one to the panes as a status-bar segment. The panes always keep full height.
pub(crate) fn frame(
    stoat: &mut Stoat,
    buf: &mut Buffer,
    scene: &mut ApcScene,
    undercurls: &mut Vec<UndercurlSpan>,
) {
    let full = stoat.size();

    if let Some(deadline) = stoat.pending_message_deadline
        && stoat.executor.now() >= deadline
    {
        stoat.pending_message = None;
        stoat.pending_message_deadline = None;
        stoat.pending_message_expiry = None;
    }

    let mode = stoat.focused_mode().to_string();
    let minimap_mode = stoat.minimap_mode();
    let minimap_enabled = minimap_mode != MinimapMode::Off;
    stoat.ensure_minimap_content_ids();

    // Single mode reserves a strip band at the window's right edge and shrinks
    // the pane layout by the strip width, so the panes never overlap the strip.
    // The band stops one row above the bottom so a status bar on that row runs
    // the full window width. The band is stamped on Stoat for the mouse handler,
    // then read back for this paint.
    stoat.single_minimap_rect = (minimap_mode == MinimapMode::Single
        && full.width >= editor::MINIMAP_MIN_PANE_COLS)
        .then(|| Rect {
            x: full.x + full.width - editor::MINIMAP_STRIP_COLS,
            y: full.y,
            width: editor::MINIMAP_STRIP_COLS,
            height: full.height.saturating_sub(1),
        });
    let single_minimap_rect = stoat.single_minimap_rect;
    let modal_overlay = modal_overlay_open(stoat);
    let size = stoat.layout_size();
    let minimap_chrome = minimap_enabled.then(|| {
        let thumb = {
            let sel = stoat.theme.get(crate::theme::scope::UI_SELECTION_EDITOR);
            let [r, g, b] = review::style_rgb(sel.bg).unwrap_or([90, 90, 110]);
            [r, g, b, 96]
        };
        MinimapChrome {
            workspace: stoat.active_workspace,
            content: &stoat.minimap_content,
            palette: stoat.minimap_class_table.palette(),
            thumb,
        }
    });

    let lsp_pending = stoat.lsp_pending_label();
    let diff_warm_busy = stoat.diff_warm_busy();

    let ws = &mut stoat.workspaces[stoat.active_workspace];

    ws.layout(size);

    let screen = crate::keymap_state::view_predicate(ws);

    let overlay_pane = if matches!(
        screen,
        Some("commits" | "rebase" | "reword" | "rebase_conflict")
    ) {
        Some(ws.panes.focus())
    } else {
        None
    };

    let workspace_name = if !ws.name.is_empty() {
        ws.name.clone()
    } else {
        ws.git_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("(unnamed)")
            .to_string()
    };

    let focused_language = {
        let focused = ws.panes.pane(ws.panes.focus());
        if let View::Editor(editor_id) = &focused.view {
            ws.editors.get(*editor_id).and_then(|editor| {
                crate::action_handlers::lsp::lsp_language_name(&ws.buffers, editor.buffer_id)
            })
        } else {
            None
        }
    };
    let lsp_servers: Vec<(String, bool)> = focused_language
        .map(|language| {
            stoat
                .lsp_registry
                .names_for_language(&language)
                .into_iter()
                .map(|name| {
                    let busy = stoat.lsp_progress.server_busy(&name);
                    (pane::lsp_short_name(&name), busy)
                })
                .collect()
        })
        .unwrap_or_default();

    let lsp_progress_entries = stoat.lsp_progress.entries_by_freshness();

    let frame = FrameCtx {
        workspace_name: &workspace_name,
        workspace_root: &ws.git_root,
        mode: &mode,
        screen,
        theme: &stoat.theme,
        pending_count: stoat.pending_count,
        lsp_status_open: stoat.lsp_status_pinned || stoat.lsp_badge_hovered,
        lsp_progress_entries: &lsp_progress_entries,
        spinner_phase: app::spinner_phase(stoat.spinner_clock),
        lsp_servers: &lsp_servers,
        diff_warm_busy,
        lsp_pending,
        lsp_message: stoat
            .lsp_message
            .as_ref()
            .map(|(typ, message)| (*typ, message.as_str())),
        status_message: stoat.pending_message.as_deref(),
        goto_word_labels: stoat.pending_goto_word.as_ref(),
        mode_badges: &stoat.settings.mode_badges,
        diagnostics: &stoat.diagnostics,
        lsp_registry: &stoat.lsp_registry,
        search_query: stoat.last_search.as_ref().map(|s| s.query.as_str()),
        line_numbers: stoat
            .settings
            .editor_line_numbers
            .unwrap_or(LineNumbers::Relative),
        wrap_mode: stoat.settings.editor_wrap.unwrap_or(WrapMode::EditorWidth),
        wrap_column: stoat.settings.editor_wrap_column.unwrap_or(80).max(1),
        inactive_dim: stoat
            .settings
            .ui_inactive_dim
            .unwrap_or(0.25)
            .clamp(0.0, 1.0) as f32,
        minimap_enabled: minimap_enabled && minimap_mode == MinimapMode::PerPane,
        minimap_chrome,
        minimap_band: single_minimap_rect,
        hover_cell: stoat.hover_cell,
        home: stoat.home.as_deref(),
        #[cfg(feature = "perf")]
        perf: PerfSegment::capture(&stoat.perf),
    };

    let split_focused = ws.panes.focus();
    let mut lsp_badge_rect: Option<Rect> = None;
    for (id, pane) in ws.panes.split_panes() {
        let is_focused = matches!(ws.focus, FocusTarget::SplitPane) && id == split_focused;
        if Some(id) == overlay_pane {
            continue;
        }
        if pane.area.width == 0 || pane.area.height == 0 {
            continue;
        }
        pane::render_pane(
            pane,
            is_focused,
            PaneCtx {
                editors: &mut ws.editors,
                buffers: &ws.buffers,
                runs: &ws.runs,
                terms: &ws.terms,
            },
            frame,
            buf,
            scene,
            undercurls,
            &mut lsp_badge_rect,
        );
    }
    stoat.lsp_badge_rect = lsp_badge_rect;
    if lsp_badge_rect.is_none() {
        stoat.lsp_badge_hovered = false;
    }

    // Single mode declares one strip over the reserved right-edge band for the
    // focused split pane's buffer. The scene re-stamps every paint, so a focus
    // switch to another buffer redeclares it. A non-editor focus leaves it empty.
    if !modal_overlay
        && let (Some(band), Some(chrome)) = (single_minimap_rect, frame.minimap_chrome)
        && let View::Editor(editor_id) = &ws.panes.pane(ws.panes.focus()).view
        && let Some(editor) = ws.editors.get(*editor_id)
        && editor.review_view.is_none()
        && !editor.diff_view
        && let Some(content) = chrome.content.get(&(chrome.workspace, editor.buffer_id))
    {
        let [tr, tg, tb, _] = chrome.thumb;
        Minimap {
            strip_id: SINGLE_MINIMAP_STRIP_ID,
            content_id: content.content_id(),
            lines_per_cell: pane::MINIMAP_LINES_PER_CELL,
            max_columns: pane::MINIMAP_MAX_COLUMNS,
            bg: [0, 0, 0, 0],
            thumb: chrome.thumb,
            thumb_border: [tr, tg, tb],
            palette: chrome.palette.to_vec(),
        }
        .render(band, buf, scene);
    }

    // Record each undercurl span's cells now, after the editor panes painted but
    // before the overlay stack, so the re-stamp can drop cells a later layer
    // repaints and never draw over an overlay covering a diagnostic.
    undercurl::snapshot_cells(buf, undercurls);

    pane::render_pane_dividers(&ws.panes.dividers(), &stoat.theme, buf, &mut *scene);

    if let Some(pane_id) = overlay_pane {
        let pane = ws.panes.pane(pane_id);
        let is_focused = matches!(ws.focus, FocusTarget::SplitPane) && ws.panes.focus() == pane_id;
        if screen == Some("commits") {
            if let Some(state) = ws.commits.as_mut() {
                commits::render_commits(pane, is_focused, state, frame, buf, &mut *scene);
            }
        } else if screen == Some("rebase") {
            if let Some(state) = ws.rebase.as_ref() {
                rebase::render_rebase(pane, is_focused, state, frame, buf, &mut *scene);
            }
        } else if screen == Some("reword") {
            let reword_ctx = ws
                .rebase_active
                .as_ref()
                .and_then(|a| a.pause.as_ref())
                .and_then(|p| match p {
                    RebasePause::Reword {
                        cherry_picked_commit,
                        original_message,
                        input,
                    } => Some((
                        cherry_picked_commit.clone(),
                        original_message.clone(),
                        input.editor_id,
                    )),
                    _ => None,
                });
            if let Some((sha, orig, editor_id)) = reword_ctx
                && let Some(editor) = ws.editors.get_mut(editor_id)
            {
                reword::render_reword(
                    pane,
                    is_focused,
                    editor,
                    &sha,
                    &orig,
                    frame,
                    buf,
                    &mut *scene,
                );
            }
        } else if screen == Some("rebase_conflict")
            && let Some(active) = ws.rebase_active.as_ref()
        {
            conflict::render_conflict(pane, is_focused, active, frame, buf, &mut *scene);
        }
    }

    for (dock_id, dock) in &ws.docks {
        if matches!(dock.visibility, DockVisibility::Hidden) {
            continue;
        }
        let is_focused = matches!(ws.focus, FocusTarget::Dock(id) if id == dock_id);
        if matches!(dock.visibility, DockVisibility::Minimized) {
            dock::render_dock_minimized(dock, is_focused, &stoat.theme, buf, &mut *scene);
        } else {
            dock::render_dock_open(
                dock,
                is_focused,
                PaneCtx {
                    editors: &mut ws.editors,
                    buffers: &ws.buffers,
                    runs: &ws.runs,
                    terms: &ws.terms,
                },
                frame,
                buf,
                &mut *scene,
            );
        }
    }
    hover::render_hover(stoat, buf, &mut *scene);
    signature_help::render_signature_help(stoat, buf, &mut *scene);
    completion::render_completion(stoat, buf, &mut *scene);
    code_action::render_code_action(stoat, buf, &mut *scene);
    rename_input::render_rename_input(stoat, buf, &mut *scene);
    symbol_picker::render_symbol_picker(stoat, buf, &mut *scene);
    let ws = &mut stoat.workspaces[stoat.active_workspace];
    badges::sync_agent_badge(&mut ws.badges, ws.agent.as_ref());
    badges::render_badges(
        &ws.badges,
        &stoat.badges,
        size,
        stoat.render_tick,
        &stoat.theme,
        buf,
    );
    if let Some(run_id) = stoat.modal_run {
        if let Some(run_state) = ws.runs.get(run_id) {
            run_pane::render_modal_run(run_state, &stoat.theme, full, buf, &mut *scene);
        }
    } else if let Some(help) = &stoat.help {
        help::render_help(
            help,
            &mode,
            ws,
            &stoat.theme,
            &stoat.settings.mode_badges,
            full,
            buf,
            &mut *scene,
        );
        cached_modal_hints(
            &mut stoat.hints_cache,
            &stoat.keymap,
            &mode,
            "help",
            &stoat.theme,
            full,
            buf,
            &mut *scene,
        );
    } else if let Some(finder) = &mut stoat.file_finder {
        file_finder::render_file_finder(finder, ws, &stoat.theme, full, buf, &mut *scene);
        cached_modal_hints(
            &mut stoat.hints_cache,
            &stoat.keymap,
            &mode,
            "finder",
            &stoat.theme,
            full,
            buf,
            &mut *scene,
        );
    } else if let Some(finder) = &mut stoat.symbol_finder {
        symbol_finder::render_symbol_finder(
            finder,
            ws,
            &stoat.theme,
            &stoat.language_registry,
            full,
            buf,
            &mut *scene,
        );
        cached_modal_hints(
            &mut stoat.hints_cache,
            &stoat.keymap,
            &mode,
            "symbols",
            &stoat.theme,
            full,
            buf,
            &mut *scene,
        );
    } else if let Some(finder) = &mut stoat.code_search {
        code_search::render_code_search(finder, ws, &stoat.theme, full, buf, &mut *scene);
        cached_modal_hints(
            &mut stoat.hints_cache,
            &stoat.keymap,
            &mode,
            "code_search",
            &stoat.theme,
            full,
            buf,
            &mut *scene,
        );
    } else if let Some(palette) = &mut stoat.command_palette {
        command_palette::render_command_palette(palette, ws, &stoat.theme, full, buf, &mut *scene);
        cached_modal_hints(
            &mut stoat.hints_cache,
            &stoat.keymap,
            &mode,
            "palette",
            &stoat.theme,
            full,
            buf,
            &mut *scene,
        );
    } else if let Some(picker) = &stoat.workspace_picker {
        workspace_picker::render_workspace_picker(picker, ws, &stoat.theme, full, buf, &mut *scene);
        let bindings = picker.hint_bindings();
        hints::render_hints(
            "picker",
            &bindings,
            None,
            &stoat.theme,
            full,
            buf,
            &mut *scene,
        );
    } else if let Some(modal) = &stoat.quit_all_confirm {
        quit_all_confirm::render_quit_all_confirm(modal, &stoat.theme, full, buf, &mut *scene);
        let bindings: Vec<(&'static str, String)> = vec![
            ("y", "discard & quit".to_string()),
            ("n", "cancel".to_string()),
            ("Enter", "discard & quit".to_string()),
            ("Esc", "cancel".to_string()),
        ];
        hints::render_hints(
            "quit",
            &bindings,
            None,
            &stoat.theme,
            full,
            buf,
            &mut *scene,
        );
    } else if let Some(picker) = &stoat.jumplist_picker {
        jumplist_picker::render_jumplist_picker(picker, &stoat.theme, full, buf, &mut *scene);
        let bindings = picker.hint_bindings();
        hints::render_hints(
            "jumplist",
            &bindings,
            None,
            &stoat.theme,
            full,
            buf,
            &mut *scene,
        );
    } else if let Some(picker) = &stoat.diagnostics_picker {
        diagnostics_picker::render_diagnostics_picker(
            picker,
            &ws.git_root,
            &stoat.theme,
            full,
            buf,
            &mut *scene,
        );
        let bindings = picker.hint_bindings();
        hints::render_hints(
            "diagnostics",
            &bindings,
            None,
            &stoat.theme,
            full,
            buf,
            &mut *scene,
        );
    } else if let Some(picker) = &stoat.location_picker {
        location_picker::render_location_picker(
            picker,
            &ws.git_root,
            &stoat.theme,
            full,
            buf,
            &mut *scene,
        );
        let bindings = picker.hint_bindings();
        hints::render_hints(
            "locations",
            &bindings,
            None,
            &stoat.theme,
            full,
            buf,
            &mut *scene,
        );
    } else if mode != "space_pane_display"
        && (!PRIMARY_MODES.contains(&mode.as_str())
            || screen == Some("review")
            || screen == Some("conflict")
            || stoat.key_hints_visible)
    {
        // The space_pane_display chord paints its own digit badges below, and
        // the auto-shown hint box would only stack an eleven-row overlay over
        // them. The guard above keeps it suppressed even when the `?` key-hints
        // toggle is on.
        //
        // `from_stoat` would take a whole `&Stoat`, but `ws` already holds a
        // mutable borrow of the active workspace, so read the flags directly.
        let flags = Flags {
            rebase_exec: ws.rebase_active.is_some(),
        };
        let token = cursor_token(ws);
        let focus = focus_flags(ws, &stoat.diagnostics, &stoat.lsp_registry);
        let key = hints_cache_key(&mode, screen, &flags, token, &focus, None);

        if stoat.hints_cache.as_ref().map(|c| c.key) != Some(key) {
            // The review screen rides on normal mode, so scope to its own `view
            // == review` bindings. A chord sub-mode owns its whole mode, so take
            // them all.
            let state = StoatKeymapState::with_flags(&mode, flags)
                .with_view(screen)
                .with_token(token)
                .with_focus_flags(focus);
            let raw = if screen == Some("review") {
                stoat.keymap.scoped_bindings(&state, "view", "review")
            } else if screen == Some("conflict") {
                stoat.keymap.scoped_bindings(&state, "view", "conflict")
            } else {
                stoat.keymap.active_bindings(&state)
            };
            let bindings: Vec<(&str, String)> = raw
                .iter()
                .map(|(key, actions)| {
                    (
                        key.as_str(),
                        actions.first().map(action_display_desc).unwrap_or_default(),
                    )
                })
                .collect();
            stoat.hints_cache = Some(hints::HintsCache {
                key,
                rows: hints::group_by_action(&bindings),
            });
        }

        // The review footer is cached against the session version so its
        // per-chunk progress walk reruns only when the session mutates. The
        // conflict footer is cheap enough to rebuild each frame.
        if screen == Some("review") {
            match ws.review.as_ref() {
                Some(session) => {
                    let version = session.version;
                    if stoat.review_footer_cache.as_ref().map(|c| c.0) != Some(version) {
                        let footer = build_review_footer(session, &stoat.theme);
                        stoat.review_footer_cache = Some((version, Some(footer)));
                    }
                },
                None => stoat.review_footer_cache = None,
            }
        }
        let conflict_footer = if screen == Some("conflict") {
            let conflict_state = match ws.panes.pane(ws.panes.focus()).view {
                View::Editor(id) => ws.editors.get(id).and_then(|e| e.conflict_view.as_ref()),
                _ => None,
            };
            conflict_state.map(|state| {
                let total = state.doc.chunks.len();
                let resolved = state.doc.chunks.iter().filter(|c| c.auto.is_some()).count();
                let text = format!(
                    "file {}/{} {} · {} conflicts · {} resolved",
                    state.file_index + 1,
                    state.file_count,
                    state.rel_path,
                    total,
                    resolved,
                );
                hints::HintsFooter {
                    text,
                    style: stoat.theme.get(crate::theme::scope::UI_TEXT),
                }
            })
        } else {
            None
        };
        let footer = if screen == Some("review") {
            stoat
                .review_footer_cache
                .as_ref()
                .and_then(|c| c.1.as_ref())
        } else {
            conflict_footer.as_ref()
        };

        let hint_label = match screen {
            Some("review") => "review",
            Some("conflict") => "conflict",
            _ => mode.as_str(),
        };
        let rows = &stoat
            .hints_cache
            .as_ref()
            .expect("cache populated above")
            .rows;
        hints::render_hints_grouped(
            hint_label,
            rows,
            footer,
            &stoat.theme,
            full,
            buf,
            &mut *scene,
        );
    }

    if mode == "space_pane_display" {
        render_pane_id_badges(&stoat.theme, ws, buf, scene);
    }
}

/// Hash the keymap-state inputs that decide which bindings are active into a
/// cache key. An unchanged key means the same hints list, so the binding walk
/// and regrouping can be skipped.
fn hints_cache_key(
    mode: &str,
    screen: Option<&str>,
    flags: &Flags,
    token: Option<Option<crate::lsp::LspSymbolKind>>,
    focus: &FocusFlags,
    modal: Option<&str>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    mode.hash(&mut hasher);
    screen.hash(&mut hasher);
    flags.hash(&mut hasher);
    token.hash(&mut hasher);
    focus.hash(&mut hasher);
    modal.hash(&mut hasher);
    hasher.finish()
}

/// Paint a modal's hint box, reusing `cache` when its mode and modal are
/// unchanged so the scoped keymap walk and regrouping run only on a key miss.
#[allow(clippy::too_many_arguments)]
fn cached_modal_hints(
    cache: &mut Option<hints::HintsCache>,
    keymap: &crate::keymap::Keymap,
    mode: &str,
    modal: &'static str,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let key = hints_cache_key(
        mode,
        None,
        &Flags::default(),
        None,
        &FocusFlags::default(),
        Some(modal),
    );
    if cache.as_ref().map(|c| c.key) != Some(key) {
        let state = StoatKeymapState::with_flags(mode, Flags::default()).with_modal(modal);
        let raw = keymap.scoped_bindings(&state, "modal", modal);
        let bindings: Vec<(&str, String)> = raw
            .iter()
            .map(|(key, actions)| {
                (
                    key.as_str(),
                    actions.first().map(action_display_desc).unwrap_or_default(),
                )
            })
            .collect();
        *cache = Some(hints::HintsCache {
            key,
            rows: hints::group_by_action(&bindings),
        });
    }
    let rows = &cache.as_ref().expect("cache populated above").rows;
    hints::render_hints_grouped(modal, rows, None, theme, area, buf, scene);
}

/// Build the review-screen hints footer from the session progress and theme.
fn build_review_footer(
    session: &crate::review_session::ReviewSession,
    theme: &crate::theme::Theme,
) -> hints::HintsFooter {
    let p = session.progress();
    let complete = session.is_complete();
    let text = if complete {
        format!("all {} reviewed", p.total)
    } else {
        let current = p.current_index.unwrap_or(0);
        format!(
            "{}/{} · {} staged · {} unstaged · {} pending",
            current, p.total, p.staged, p.unstaged, p.pending
        )
    };
    let style = if complete {
        theme.get(crate::theme::scope::UI_BADGE_COMPLETE)
    } else {
        theme.get(crate::theme::scope::UI_TEXT)
    };
    hints::HintsFooter { text, style }
}

/// Paint a large digit badge centered on each split pane while the
/// `space_pane_display` chord is active, so a pane can be focused by its number.
///
/// Panes are numbered 1-9 then 0 for the tenth in `split_panes` layout order,
/// and panes past the tenth get no badge. The focused pane's badge is inverted
/// (fill and mark swapped) so the current pane reads highlighted. Each badge is
/// a bold [`Popover`], so a plain terminal draws the cell-fallback box and
/// stoatty floats the scaled bold glyph.
fn render_pane_id_badges(
    theme: &crate::theme::Theme,
    ws: &Workspace,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let accent = review::style_rgb(theme.get(crate::theme::scope::UI_SELECTION_EDITOR).bg)
        .unwrap_or([90, 90, 110]);
    let background =
        review::style_rgb(theme.get(crate::theme::scope::UI_BACKGROUND).bg).unwrap_or([40, 44, 52]);
    let focused = ws.panes.focus();

    for (i, (pane_id, pane)) in ws.panes.split_panes().enumerate().take(10) {
        let Some(digit) = char::from_digit((i as u32 + 1) % 10, 10) else {
            continue;
        };
        if pane.area.width == 0 || pane.area.height == 0 {
            continue;
        }
        let scale = pane
            .area
            .width
            .saturating_sub(2)
            .min(pane.area.height.saturating_sub(2))
            .clamp(1, 4);
        let side = scale + 2;
        let rect = Rect::new(
            pane.area.x + pane.area.width.saturating_sub(side) / 2,
            pane.area.y + pane.area.height.saturating_sub(side) / 2,
            side,
            side,
        );

        let (fill, mark) = if pane_id == focused {
            (accent, background)
        } else {
            (background, accent)
        };
        let digit = digit.to_string();
        Popover {
            fill,
            border: mark,
            content_fg: mark,
            scale: scale as u8,
            offset: [0, 0],
            bold: true,
            content: &digit,
        }
        .render(rect, buf, scene);
    }
}

#[cfg(all(test, feature = "perf"))]
mod perf_tests {
    use super::{perf_label, PerfSegment};
    use crate::perf::PerfStats;
    use std::time::Duration;

    #[test]
    fn capture_is_none_until_a_frame_is_painted() {
        assert!(PerfSegment::capture(&PerfStats::default()).is_none());
    }

    #[test]
    fn capture_reads_paint_and_input_percentiles_in_micros() {
        let mut perf = PerfStats::default();
        perf.record_paint(Duration::from_micros(123));
        perf.record_input_to_publish(Duration::from_micros(456));
        let seg = PerfSegment::capture(&perf).expect("data recorded");
        assert_eq!(seg.last_paint_us, 123);
        assert_eq!(seg.p95_input_us, 456);
    }

    #[test]
    fn perf_label_formats_both_values() {
        let seg = PerfSegment {
            last_paint_us: 12,
            p95_input_us: 34,
        };
        assert_eq!(perf_label(seg), " paint 12us in-p95 34us ");
    }
}

#[cfg(test)]
mod lsp_filter_tests {
    use crate::{lsp::LspSymbolKind, test_harness::TestHarness};
    use std::sync::Arc;

    /// Render one frame and flatten the painted cells into searchable text.
    fn box_text(h: &mut TestHarness) -> String {
        let buf = h.render_composited();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Open "Foo bar" and return its buffer id and focused-editor workspace.
    fn open_foo_bar(h: &mut TestHarness) -> crate::buffer::BufferId {
        let root = std::path::PathBuf::from("/lsp");
        let path = root.join("a.rs");
        h.fake_fs().insert_file(&path, b"Foo bar");
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFile { path });
        h.settle();
        let ws = h.stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            crate::pane::View::Editor(id) => ws.editors[id].buffer_id,
            _ => panic!("focused pane is not an editor"),
        }
    }

    #[test]
    fn space_lsp_box_filters_rows_by_cursor_symbol_kind() {
        let mut h = TestHarness::with_size(150, 50);
        let buffer_id = open_foo_bar(&mut h);

        {
            let ws = h.stoat.active_workspace_mut();
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            let snapshot = buffer.read().unwrap().snapshot.clone();
            let start = |o| snapshot.anchors_at_batch(&[o], stoat_text::Bias::Right)[0];
            let end = |o| snapshot.anchors_at_batch(&[o], stoat_text::Bias::Left)[0];
            let kinds = Arc::from(vec![
                (start(0usize)..end(3usize), LspSymbolKind::Trait),
                (start(4usize)..end(7usize), LspSymbolKind::Function),
            ]);
            ws.buffers.store_lsp_symbol_kinds(buffer_id, kinds);
        }

        // The cursor starts on "Foo", the trait.
        h.type_keys("space l");
        let over_trait = box_text(&mut h);
        assert!(
            over_trait.contains("implementor of the trait"),
            "the implementors row shows over a trait"
        );
        assert!(
            !over_trait.contains("caller of the symbol"),
            "the caller row is hidden over a trait"
        );

        // Move onto "bar", the function.
        h.type_keys("escape");
        h.type_keys("l l l l");
        h.type_keys("space l");
        let over_function = box_text(&mut h);
        assert!(
            over_function.contains("caller of the symbol"),
            "the caller row shows over a function"
        );
        assert!(
            !over_function.contains("implementor of the trait"),
            "the implementors row is hidden over a function"
        );
    }

    #[test]
    fn space_lsp_box_shows_all_rows_without_an_index() {
        let mut h = TestHarness::with_size(150, 50);
        open_foo_bar(&mut h);

        h.type_keys("space l");
        let text = box_text(&mut h);
        assert!(
            text.contains("implementor of the trait") && text.contains("caller of the symbol"),
            "a buffer with no symbol-kind index shows every row"
        );
    }

    #[test]
    fn space_lsp_box_gates_lsp_and_diagnostic_rows() {
        use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

        let mut h = TestHarness::with_size(150, 50);

        // The initial scratch buffer has no language and no path, so no server
        // serves it and it carries no diagnostics.
        h.type_keys("space l");
        let scratch = box_text(&mut h);
        assert!(
            scratch.contains("caller of the symbol"),
            "token rows fail open without an index"
        );
        assert!(
            !scratch.contains("format document via LSP"),
            "the format row is hidden without a language server"
        );
        assert!(
            !scratch.contains("jump to the next diagnostic"),
            "the diagnostics row is hidden without a diagnostic"
        );

        // The sole fake client serves the rust buffer, so lsp rows appear.
        h.type_keys("escape");
        open_foo_bar(&mut h);
        h.type_keys("space l");
        let served = box_text(&mut h);
        assert!(
            served.contains("format document via LSP"),
            "the format row shows once a server serves the buffer"
        );
        assert!(
            !served.contains("jump to the next diagnostic"),
            "the diagnostics row stays hidden until a diagnostic exists"
        );

        h.type_keys("escape");
        h.stoat.diagnostics.replace_for_path(
            std::path::PathBuf::from("/lsp/a.rs"),
            vec![Diagnostic {
                range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                severity: Some(DiagnosticSeverity::ERROR),
                code: None,
                code_description: None,
                source: None,
                message: String::new(),
                related_information: None,
                tags: None,
                data: None,
            }],
        );
        h.type_keys("space l");
        let with_diag = box_text(&mut h);
        assert!(
            with_diag.contains("jump to the next diagnostic"),
            "the diagnostics row shows once a diagnostic is seeded"
        );
    }
}
