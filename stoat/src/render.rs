pub(crate) mod badges;
pub(crate) mod chrome;
pub(crate) mod code_action;
pub(crate) mod command_palette;
pub(crate) mod commits;
pub(crate) mod completion;
pub(crate) mod conflict;
pub(crate) mod diagnostics_picker;
pub(crate) mod dock;
pub(crate) mod editor;
pub(crate) mod file_finder;
pub(crate) mod global_search;
pub(crate) mod help;
pub(crate) mod hints;
pub(crate) mod hover;
pub(crate) mod jumplist_picker;
pub(crate) mod layout;
pub(crate) mod location_picker;
pub(crate) mod pane;
pub(crate) mod picker;
pub(crate) mod quit_all_confirm;
pub(crate) mod rebase;
pub(crate) mod rename_input;
pub(crate) mod review;
pub(crate) mod reword;
pub(crate) mod run_pane;
pub(crate) mod sanitize;
pub(crate) mod signature_help;
pub(crate) mod symbol_picker;
pub(crate) mod term_pane;
pub(crate) mod text;
pub(crate) mod undercurl;
pub(crate) mod workspace_picker;
pub(crate) mod workspace_symbol_picker;

use self::undercurl::UndercurlSpan;
use crate::{
    app::Stoat,
    buffer::BufferId,
    buffer_registry::BufferRegistry,
    editor_state::{EditorId, EditorState},
    keymap_state::{action_display_desc, Flags, StoatKeymapState},
    minimap::MinimapContent,
    pane::{DockVisibility, FocusTarget},
    rebase::RebasePause,
    run::{RunId, RunState},
    term_session::{TermId, TermSession},
    workspace::WorkspaceId,
};
use ratatui::{buffer::Buffer, layout::Rect};
use slotmap::SlotMap;
use std::{collections::HashMap, path::Path};
use stoat_config::LineNumbers;
use stoatty_widgets::ApcScene;

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
    /// Most recently updated in-progress LSP work-done entry, if any.
    /// Painted in the right side of the status bar so users see
    /// "rust-analyzer indexing" / "checking" progress.
    pub(crate) lsp_progress: Option<&'a crate::lsp::progress::LspProgressEntry>,
    /// Freshest `window/showMessage` text, painted in the right side of
    /// the status bar. `MessageType::ERROR` is styled as an error.
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
    /// Most-recently submitted in-buffer search query. When `Some`,
    /// every editor pane paints visible matches with the
    /// `ui.search.match` style so users see all hits at once.
    pub(crate) search_query: Option<&'a str>,
    /// Whether stoat is running inside stoatty. When set, the focused
    /// document editor delegates its primary cursor to the terminal cursor
    /// (which stoatty eases) instead of painting a styled grid cell.
    pub(crate) stoatty: bool,
    /// How document editor panes number the gutter, resolved from
    /// `editor.line_numbers` (default [`LineNumbers::Relative`]).
    /// [`LineNumbers::Off`] keeps the diagnostic-only gutter column.
    pub(crate) line_numbers: LineNumbers,
    /// Whether editor panes reserve the right-edge minimap strip, resolved from
    /// [`crate::app::Stoat::minimap_enabled`]. Only takes effect under stoatty.
    pub(crate) minimap_enabled: bool,
    /// The lookup and colors a pane needs to declare its minimap strip, `Some`
    /// only when the strip is active (under stoatty with the minimap enabled).
    pub(crate) minimap_chrome: Option<MinimapChrome<'a>>,
    /// Terminal cell the mouse last rested over, or `None` when it has not
    /// moved over a pane. The focused editor resolves the diagnostic under it
    /// to raise a hover popover.
    pub(crate) hover_cell: Option<(u16, u16)>,
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

/// Reserves the bottom row for the pane status bar so the hints overlay
/// does not paint over it.
pub(crate) fn hints_overlay_area(size: Rect) -> Rect {
    Rect {
        x: size.x,
        y: size.y,
        width: size.width,
        height: size.height.saturating_sub(1),
    }
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

    let size = full;

    let mode = stoat.focused_mode().to_string();
    let minimap_enabled = stoat.minimap_enabled();
    stoat.ensure_minimap_content_ids();
    let minimap_chrome = (stoat.stoatty && minimap_enabled).then(|| {
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

    let ws = &mut stoat.workspaces[stoat.active_workspace];

    ws.layout(size);

    let screen = crate::keymap_state::view_predicate(ws);
    let overlay_pane = if matches!(screen, Some("commits" | "rebase" | "reword" | "conflict")) {
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

    let frame = FrameCtx {
        workspace_name: &workspace_name,
        workspace_root: &ws.git_root,
        mode: &mode,
        screen,
        theme: &stoat.theme,
        pending_count: stoat.pending_count,
        lsp_progress: stoat.lsp_progress.current(),
        lsp_message: stoat
            .lsp_message
            .as_ref()
            .map(|(typ, message)| (*typ, message.as_str())),
        status_message: stoat.pending_message.as_deref(),
        goto_word_labels: stoat.pending_goto_word.as_ref(),
        mode_badges: &stoat.settings.mode_badges,
        diagnostics: &stoat.diagnostics,
        search_query: stoat.last_search.as_ref().map(|s| s.query.as_str()),
        stoatty: stoat.stoatty,
        line_numbers: stoat
            .settings
            .editor_line_numbers
            .unwrap_or(LineNumbers::Relative),
        minimap_enabled,
        minimap_chrome,
        hover_cell: stoat.hover_cell,
        #[cfg(feature = "perf")]
        perf: PerfSegment::capture(&stoat.perf),
    };

    let split_focused = ws.panes.focus();
    for (id, pane) in ws.panes.split_panes() {
        let is_focused = matches!(ws.focus, FocusTarget::SplitPane(_)) && id == split_focused;
        if Some(id) == overlay_pane {
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
        );
    }

    // Record each undercurl span's cells now, after the editor panes painted but
    // before the overlay stack, so the re-stamp can drop cells a later layer
    // repaints and never draw over an overlay covering a diagnostic.
    undercurl::snapshot_cells(buf, undercurls);

    pane::render_pane_dividers(
        &ws.panes.dividers(),
        &stoat.theme,
        buf,
        stoat.stoatty.then_some(&mut *scene),
    );

    if let Some(pane_id) = overlay_pane {
        let pane = ws.panes.pane(pane_id);
        let is_focused = matches!(ws.focus, FocusTarget::SplitPane(id) if id == pane_id);
        if screen == Some("commits") {
            if let Some(state) = ws.commits.as_mut() {
                commits::render_commits(
                    pane,
                    is_focused,
                    state,
                    frame,
                    buf,
                    frame.stoatty.then_some(&mut *scene),
                );
            }
        } else if screen == Some("rebase") {
            if let Some(state) = ws.rebase.as_ref() {
                rebase::render_rebase(
                    pane,
                    is_focused,
                    state,
                    frame,
                    buf,
                    frame.stoatty.then_some(&mut *scene),
                );
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
                    frame.stoatty.then_some(&mut *scene),
                );
            }
        } else if screen == Some("conflict")
            && let Some(active) = ws.rebase_active.as_ref()
        {
            conflict::render_conflict(
                pane,
                is_focused,
                active,
                frame,
                buf,
                frame.stoatty.then_some(&mut *scene),
            );
        }
    }

    for (dock_id, dock) in &ws.docks {
        if matches!(dock.visibility, DockVisibility::Hidden) {
            continue;
        }
        let is_focused = matches!(ws.focus, FocusTarget::Dock(id) if id == dock_id);
        if matches!(dock.visibility, DockVisibility::Minimized) {
            dock::render_dock_minimized(
                dock,
                is_focused,
                &stoat.theme,
                buf,
                frame.stoatty.then_some(&mut *scene),
            );
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
                frame.stoatty.then_some(&mut *scene),
            );
        }
    }
    hover::render_hover(stoat, buf, stoat.stoatty.then_some(&mut *scene));
    signature_help::render_signature_help(stoat, buf, stoat.stoatty.then_some(&mut *scene));
    completion::render_completion(stoat, buf, stoat.stoatty.then_some(&mut *scene));
    code_action::render_code_action(stoat, buf, stoat.stoatty.then_some(&mut *scene));
    rename_input::render_rename_input(stoat, buf, stoat.stoatty.then_some(&mut *scene));
    symbol_picker::render_symbol_picker(stoat, buf, stoat.stoatty.then_some(&mut *scene));
    workspace_symbol_picker::render_workspace_symbol(
        stoat,
        buf,
        stoat.stoatty.then_some(&mut *scene),
    );
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
            run_pane::render_modal_run(
                run_state,
                &stoat.theme,
                size,
                buf,
                stoat.stoatty.then_some(&mut *scene),
            );
        }
    } else if let Some(help) = &stoat.help {
        help::render_help(
            help,
            &mode,
            ws,
            &stoat.theme,
            &stoat.settings.mode_badges,
            size,
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
        let state = StoatKeymapState::with_flags(&mode, Flags::default()).with_modal("help");
        let raw = stoat.keymap.scoped_bindings(&state, "modal", "help");
        let bindings: Vec<_> = raw
            .iter()
            .map(|(key, actions)| {
                let desc = actions.first().map(action_display_desc).unwrap_or_default();
                (key.as_str(), desc)
            })
            .collect();
        hints::render_hints(
            "help",
            &bindings,
            None,
            &stoat.theme,
            hints_overlay_area(size),
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
    } else if let Some(finder) = &mut stoat.file_finder {
        file_finder::render_file_finder(
            finder,
            ws,
            &stoat.theme,
            size,
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
        let state = StoatKeymapState::with_flags(&mode, Flags::default()).with_modal("finder");
        let raw = stoat.keymap.scoped_bindings(&state, "modal", "finder");
        let bindings: Vec<_> = raw
            .iter()
            .map(|(key, actions)| {
                let desc = actions.first().map(action_display_desc).unwrap_or_default();
                (key.as_str(), desc)
            })
            .collect();
        hints::render_hints(
            "finder",
            &bindings,
            None,
            &stoat.theme,
            hints_overlay_area(size),
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
    } else if let Some(palette) = &mut stoat.command_palette {
        command_palette::render_command_palette(
            palette,
            ws,
            &stoat.theme,
            size,
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
        let state = StoatKeymapState::with_flags(&mode, Flags::default()).with_modal("palette");
        let raw = stoat.keymap.scoped_bindings(&state, "modal", "palette");
        let bindings: Vec<_> = raw
            .iter()
            .map(|(key, actions)| {
                let desc = actions.first().map(action_display_desc).unwrap_or_default();
                (key.as_str(), desc)
            })
            .collect();
        hints::render_hints(
            "palette",
            &bindings,
            None,
            &stoat.theme,
            hints_overlay_area(size),
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
    } else if let Some(picker) = &stoat.workspace_picker {
        workspace_picker::render_workspace_picker(
            picker,
            &stoat.theme,
            size,
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
        let bindings = picker.hint_bindings();
        hints::render_hints(
            "picker",
            &bindings,
            None,
            &stoat.theme,
            hints_overlay_area(size),
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
    } else if let Some(modal) = &stoat.quit_all_confirm {
        quit_all_confirm::render_quit_all_confirm(
            modal,
            &stoat.theme,
            size,
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
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
            hints_overlay_area(size),
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
    } else if let Some(picker) = &stoat.jumplist_picker {
        jumplist_picker::render_jumplist_picker(
            picker,
            &stoat.theme,
            size,
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
        let bindings = picker.hint_bindings();
        hints::render_hints(
            "jumplist",
            &bindings,
            None,
            &stoat.theme,
            hints_overlay_area(size),
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
    } else if let Some(picker) = &stoat.diagnostics_picker {
        diagnostics_picker::render_diagnostics_picker(
            picker,
            &ws.git_root,
            &stoat.theme,
            size,
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
        let bindings = picker.hint_bindings();
        hints::render_hints(
            "diagnostics",
            &bindings,
            None,
            &stoat.theme,
            hints_overlay_area(size),
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
    } else if let Some(picker) = &stoat.location_picker {
        location_picker::render_location_picker(
            picker,
            &ws.git_root,
            &stoat.theme,
            size,
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
        let bindings = picker.hint_bindings();
        hints::render_hints(
            "locations",
            &bindings,
            None,
            &stoat.theme,
            hints_overlay_area(size),
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
    } else if let Some(picker) = &stoat.global_search {
        let git_root = ws.git_root.clone();
        global_search::render_global_search(
            picker,
            &git_root,
            &stoat.theme,
            size,
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
        let bindings = picker.hint_bindings();
        hints::render_hints(
            "global-search",
            &bindings,
            None,
            &stoat.theme,
            hints_overlay_area(size),
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
    } else if !PRIMARY_MODES.contains(&mode.as_str())
        || screen == Some("review")
        || stoat.key_hints_visible
    {
        // `from_stoat` would take a whole `&Stoat`, but `ws` already holds a
        // mutable borrow of the active workspace, so read the flags directly.
        let state = StoatKeymapState::with_flags(
            &mode,
            Flags {
                rebase_exec: ws.rebase_active.is_some(),
            },
        )
        .with_view(screen);
        // The review screen rides on normal mode, so scope to its own `view ==
        // review` bindings. A chord sub-mode owns its whole mode, so take them all.
        let raw = if screen == Some("review") {
            stoat.keymap.scoped_bindings(&state, "view", "review")
        } else {
            stoat.keymap.active_bindings(&state)
        };
        let bindings: Vec<_> = raw
            .iter()
            .map(|(key, actions)| {
                let desc = actions.first().map(action_display_desc).unwrap_or_default();
                (key.as_str(), desc)
            })
            .collect();
        let footer = if screen == Some("review") {
            ws.review.as_ref().map(|session| {
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
                    stoat.theme.get(crate::theme::scope::UI_BADGE_COMPLETE)
                } else {
                    stoat.theme.get(crate::theme::scope::UI_TEXT)
                };
                hints::HintsFooter { text, style }
            })
        } else {
            None
        };
        let hint_label = if screen == Some("review") {
            "review"
        } else {
            mode.as_str()
        };
        hints::render_hints(
            hint_label,
            &bindings,
            footer.as_ref(),
            &stoat.theme,
            hints_overlay_area(size),
            buf,
            stoat.stoatty.then_some(&mut *scene),
        );
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
