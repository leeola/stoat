pub(crate) mod badges;
pub(crate) mod code_action;
pub(crate) mod command_palette;
pub(crate) mod commits;
pub(crate) mod completion;
pub(crate) mod conflict;
pub(crate) mod dock;
pub(crate) mod editor;
pub(crate) mod file_finder;
pub(crate) mod global_search;
pub(crate) mod help;
pub(crate) mod hints;
pub(crate) mod hover;
pub(crate) mod layout;
pub(crate) mod pane;
pub(crate) mod rebase;
pub(crate) mod rename_input;
pub(crate) mod review;
pub(crate) mod reword;
pub(crate) mod run_pane;
pub(crate) mod sanitize;
pub(crate) mod symbol_picker;
pub(crate) mod text;
pub(crate) mod workspace_symbol_picker;

use crate::{
    app::Stoat,
    buffer_registry::BufferRegistry,
    editor_state::{EditorId, EditorState},
    keymap_state::{action_display_desc, StoatKeymapState},
    pane::{DockVisibility, FocusTarget},
    rebase::RebasePause,
    run::{RunId, RunState},
};
use ratatui::{buffer::Buffer, layout::Rect};
use slotmap::SlotMap;
use std::path::Path;

pub(crate) struct PaneCtx<'a> {
    pub(crate) editors: &'a mut SlotMap<EditorId, EditorState>,
    pub(crate) buffers: &'a BufferRegistry,
    pub(crate) runs: &'a SlotMap<RunId, RunState>,
}

/// Ambient workspace and frame state shared across render functions. Bundled
/// so individual render functions stay under the `clippy::too_many_arguments`
/// threshold; every field is a cheap borrow or `Copy`.
#[derive(Clone, Copy)]
pub(crate) struct FrameCtx<'a> {
    pub(crate) workspace_name: &'a str,
    pub(crate) workspace_root: &'a Path,
    pub(crate) mode: &'a str,
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
}

pub(crate) const PRIMARY_MODES: &[&str] = &[
    "normal",
    "insert",
    "prompt",
    "run",
    "commits",
    "rebase",
    "reword",
    "reword_insert",
    "conflict",
];

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
pub(crate) fn frame(stoat: &mut Stoat, buf: &mut Buffer) {
    let size = stoat.size();
    let ws = &mut stoat.workspaces[stoat.active_workspace];

    ws.layout(size);

    let commits_mode = stoat.mode == "commits";
    let rebase_mode = stoat.mode == "rebase";
    let reword_mode = stoat.mode == "reword" || stoat.mode == "reword_insert";
    let conflict_mode = stoat.mode == "conflict";
    let overlay_pane = if (commits_mode && ws.commits.is_some())
        || (rebase_mode && ws.rebase.is_some())
        || ((reword_mode || conflict_mode) && ws.rebase_active.is_some())
    {
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
        mode: &stoat.mode,
        theme: &stoat.theme,
        pending_count: stoat.pending_count,
        lsp_progress: stoat.lsp_progress.current(),
        goto_word_labels: stoat.pending_goto_word.as_ref(),
        mode_badges: &stoat.settings.mode_badges,
        diagnostics: &stoat.diagnostics,
        search_query: stoat.last_search.as_ref().map(|s| s.query.as_str()),
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
            },
            frame,
            buf,
        );
    }

    pane::render_pane_dividers(&ws.panes.dividers(), &stoat.theme, buf);

    if let Some(pane_id) = overlay_pane {
        let pane = ws.panes.pane(pane_id);
        let is_focused = matches!(ws.focus, FocusTarget::SplitPane(id) if id == pane_id);
        if commits_mode {
            if let Some(state) = ws.commits.as_mut() {
                commits::render_commits(pane, is_focused, state, frame, buf);
            }
        } else if rebase_mode {
            if let Some(state) = ws.rebase.as_ref() {
                rebase::render_rebase(pane, is_focused, state, frame, buf);
            }
        } else if reword_mode {
            let reword_ctx = ws
                .rebase_active
                .as_ref()
                .and_then(|a| a.pause.as_ref())
                .and_then(|p| match p {
                    RebasePause::Reword {
                        cherry_picked_commit,
                        original_message,
                    } => Some((cherry_picked_commit.clone(), original_message.clone())),
                    _ => None,
                });
            let editor_id = ws.reword_input.as_ref().map(|i| i.editor_id);
            if let (Some((sha, orig)), Some(editor_id)) = (reword_ctx, editor_id) {
                if let Some(editor) = ws.editors.get_mut(editor_id) {
                    reword::render_reword(pane, is_focused, editor, &sha, &orig, frame, buf);
                }
            }
        } else if conflict_mode {
            if let Some(active) = ws.rebase_active.as_ref() {
                conflict::render_conflict(pane, is_focused, active, frame, buf);
            }
        }
    }

    for (dock_id, dock) in &ws.docks {
        if matches!(dock.visibility, DockVisibility::Hidden) {
            continue;
        }
        let is_focused = matches!(ws.focus, FocusTarget::Dock(id) if id == dock_id);
        if matches!(dock.visibility, DockVisibility::Minimized) {
            dock::render_dock_minimized(dock, is_focused, &stoat.theme, buf);
        } else {
            dock::render_dock_open(
                dock,
                is_focused,
                PaneCtx {
                    editors: &mut ws.editors,
                    buffers: &ws.buffers,
                    runs: &ws.runs,
                },
                frame,
                buf,
            );
        }
    }
    hover::render_hover(stoat, buf);
    completion::render_completion(stoat, buf);
    code_action::render_code_action(stoat, buf);
    rename_input::render_rename_input(stoat, buf);
    symbol_picker::render_symbol_picker(stoat, buf);
    workspace_symbol_picker::render_workspace_symbol(stoat, buf);
    let ws = &mut stoat.workspaces[stoat.active_workspace];
    badges::render_badges(
        &ws.badges,
        &stoat.badges,
        size,
        stoat.render_tick,
        &stoat.theme,
        buf,
    );
    if let Some(help) = &stoat.help {
        help::render_help(
            help,
            &stoat.mode,
            ws,
            &stoat.theme,
            &stoat.settings.mode_badges,
            size,
            buf,
        );
        let state = StoatKeymapState::with_flags(&stoat.mode, false, true, false);
        let raw = stoat.keymap.scoped_bindings(&state, "help_open");
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
        );
    } else if let Some(finder) = &mut stoat.file_finder {
        file_finder::render_file_finder(
            finder,
            ws,
            &*stoat.fs_host,
            &stoat.language_registry,
            &stoat.theme,
            size,
            buf,
        );
        let state = StoatKeymapState::with_flags(&stoat.mode, false, false, true);
        let raw = stoat.keymap.scoped_bindings(&state, "finder_open");
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
        );
    } else if let Some(palette) = &mut stoat.command_palette {
        command_palette::render_command_palette(palette, ws, &stoat.theme, size, buf);
        let state = StoatKeymapState::with_flags(&stoat.mode, true, false, false);
        let raw = stoat.keymap.scoped_bindings(&state, "palette_open");
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
        );
    } else if let Some(picker) = &stoat.global_search {
        let git_root = ws.git_root.clone();
        global_search::render_global_search(picker, &git_root, &stoat.theme, size, buf);
        let bindings = picker.hint_bindings();
        hints::render_hints(
            "global-search",
            &bindings,
            None,
            &stoat.theme,
            hints_overlay_area(size),
            buf,
        );
    } else if !PRIMARY_MODES.contains(&stoat.mode.as_str()) {
        let state = StoatKeymapState::new(&stoat.mode);
        let raw = stoat.keymap.active_bindings(&state);
        let bindings: Vec<_> = raw
            .iter()
            .map(|(key, actions)| {
                let desc = actions.first().map(action_display_desc).unwrap_or_default();
                (key.as_str(), desc)
            })
            .collect();
        let footer = if stoat.mode == "review" {
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
        hints::render_hints(
            &stoat.mode,
            &bindings,
            footer.as_ref(),
            &stoat.theme,
            hints_overlay_area(size),
            buf,
        );
    }
}
