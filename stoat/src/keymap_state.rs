use crate::{
    action_handlers,
    app::{ModalKind, Stoat},
    buffer::BufferId,
    diagnostics::DiagnosticSet,
    editor_state::EditorState,
    keymap::{KeymapState, ResolvedAction, ResolvedArg, StateValue},
    lsp::{registry::LspRegistry, LspSymbolKind},
    pane::{FocusTarget, View},
    rebase::RebasePause,
    workspace::Workspace,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use stoat_action::Action;

/// The predicate field names [`StoatKeymapState`] derives itself, which a
/// `SetVar` user variable may not shadow.
pub(crate) const BUILTIN_FIELDS: &[&str] = &[
    "mode",
    "pane",
    "view",
    "modal",
    "rebase_exec",
    "token",
    "token_known",
    "lsp",
    "lang",
    "diags",
    "has_selection",
    "modified",
];

/// The hand-set booleans a keymap state carries besides the derived
/// `mode`/`pane`/`view`/`modal` predicates.
///
/// Passed to [`StoatKeymapState::with_flags`] so callers that cannot run the
/// full [`StoatKeymapState::from_stoat`] derivation (e.g. while holding a
/// workspace borrow) still set the fields they need without those predicates
/// rippling through the signature.
#[derive(Default, Hash)]
pub(crate) struct Flags {
    pub(crate) rebase_exec: bool,
}

pub(crate) struct StoatKeymapState<'a> {
    mode_value: StateValue,
    rebase_exec: StateValue,
    /// The focused pane's kind, absent only when there is no focus. `None` reads
    /// as an unset field, so a `pane == x` predicate is false without one.
    pane: Option<StateValue>,
    /// The focused editor's view (`file` or `diff`), present only when the
    /// focused pane is an editor.
    view: Option<StateValue>,
    /// The topmost open modal, absent when none is open. Absence lets bare
    /// `modal` read false and `modal != x` read true.
    modal: Option<StateValue>,
    /// The semantic-token kind under the cursor, absent when no index exists or
    /// the cursor sits on no token. Absence lets bare `token` read "on a known
    /// token" (false) and pairs with [`Self::token_known`] for the fail-open
    /// `!token_known || token == kind` idiom.
    token: Option<StateValue>,
    /// Whether the focused buffer has a semantic-token index at all (a server
    /// answered). `Bool(false)` when none, so `!token_known` fails token
    /// conditions open when the index is missing or still pending.
    token_known: StateValue,
    /// Whether a language server is registered for the focused buffer's language.
    lsp: StateValue,
    /// The focused buffer's tree-sitter grammar name, absent for a grammarless
    /// buffer so `lang ~ "..."` can glob-match a family and bare `lang` reads
    /// false without a grammar.
    lang: Option<StateValue>,
    /// Whether the focused buffer's path has any published diagnostics.
    diags: StateValue,
    /// Whether the newest selection is non-empty (its head and tail differ).
    has_selection: StateValue,
    /// Whether the focused buffer has unsaved edits.
    modified: StateValue,
    /// Config-defined session variables, read only after the built-in fields so
    /// a variable can never shadow one. Borrowed from the owning [`Stoat`];
    /// `None` for the flag-built states that carry no user vars.
    user_vars: Option<&'a HashMap<String, StateValue>>,
}

impl<'a> StoatKeymapState<'a> {
    #[cfg(test)]
    pub(crate) fn new(mode: &str) -> Self {
        Self::with_flags(mode, Flags::default())
    }

    pub(crate) fn with_flags(mode: &str, flags: Flags) -> Self {
        Self {
            mode_value: StateValue::String(mode.into()),
            rebase_exec: StateValue::Bool(flags.rebase_exec),
            pane: None,
            view: None,
            modal: None,
            token: None,
            token_known: StateValue::Bool(false),
            lsp: StateValue::Bool(false),
            lang: None,
            diags: StateValue::Bool(false),
            has_selection: StateValue::Bool(false),
            modified: StateValue::Bool(false),
            user_vars: None,
        }
    }

    /// Set the `modal` predicate value on an otherwise flag-built state.
    ///
    /// Lets the hint-overlay renderer construct a state that targets a specific
    /// open modal (`modal == help`) without a full [`Self::from_stoat`], which
    /// it cannot call while holding a workspace borrow.
    pub(crate) fn with_modal(mut self, modal: &str) -> Self {
        self.modal = Some(StateValue::String(modal.into()));
        self
    }

    /// Set the `view` predicate value on an otherwise flag-built state.
    ///
    /// Lets the hint-overlay renderer scope bindings to the foreground screen
    /// (`view == review`) without a full [`Self::from_stoat`], which it cannot
    /// call while holding a workspace borrow.
    pub(crate) fn with_view(mut self, view: Option<&str>) -> Self {
        self.view = view.map(|v| StateValue::String(v.into()));
        self
    }

    /// Set the `token` and `token_known` predicate values from a semantic-token
    /// lookup at the cursor (see [`cursor_token`]).
    ///
    /// `None` (no index) leaves `token` absent and `token_known` false.
    /// `Some(None)` (index present, cursor on no token) leaves `token` absent but
    /// marks the index known. `Some(Some(kind))` sets `token` to the kind's
    /// config name.
    pub(crate) fn with_token(mut self, token: Option<Option<LspSymbolKind>>) -> Self {
        let (value, known) = match token {
            None => (None, false),
            Some(None) => (None, true),
            Some(Some(kind)) => (Some(StateValue::String(kind.config_name().into())), true),
        };
        self.token = value;
        self.token_known = StateValue::Bool(known);
        self
    }

    /// Set the `lsp`/`lang`/`diags`/`has_selection`/`modified` predicate values
    /// from a focused-buffer derivation (see [`focus_flags`]).
    pub(crate) fn with_focus_flags(mut self, flags: FocusFlags) -> Self {
        self.lsp = StateValue::Bool(flags.lsp);
        self.lang = flags.lang.map(|l| StateValue::String(l.into()));
        self.diags = StateValue::Bool(flags.diags);
        self.has_selection = StateValue::Bool(flags.has_selection);
        self.modified = StateValue::Bool(flags.modified);
        self
    }

    pub(crate) fn from_stoat(stoat: &'a Stoat) -> Self {
        let ws = stoat.active_workspace();
        let flags = Flags {
            rebase_exec: ws.rebase_active.is_some(),
        };
        Self {
            pane: pane_predicate(ws).map(|s| StateValue::String(s.into())),
            view: view_predicate(ws).map(|s| StateValue::String(s.into())),
            modal: modal_predicate(stoat).map(|s| StateValue::String(s.into())),
            user_vars: Some(&stoat.user_vars),
            ..Self::with_flags(stoat.focused_mode(), flags)
        }
        .with_token(cursor_token(ws))
        .with_focus_flags(focus_flags(ws, &stoat.diagnostics, &stoat.lsp_registry))
    }
}

impl KeymapState for StoatKeymapState<'_> {
    fn get(&self, field: &str) -> Option<&StateValue> {
        match field {
            "mode" => Some(&self.mode_value),
            "rebase_exec" => Some(&self.rebase_exec),
            "pane" => self.pane.as_ref(),
            "view" => self.view.as_ref(),
            "modal" => self.modal.as_ref(),
            "token" => self.token.as_ref(),
            "token_known" => Some(&self.token_known),
            "lsp" => Some(&self.lsp),
            "lang" => self.lang.as_ref(),
            "diags" => Some(&self.diags),
            "has_selection" => Some(&self.has_selection),
            "modified" => Some(&self.modified),
            other => self.user_vars.and_then(|m| m.get(other)),
        }
    }
}

/// The [`LspSymbolKind`] under the focused editor's cursor, the non-mutating
/// counterpart of `render`'s `lsp_cursor_kind` so [`StoatKeymapState::from_stoat`]
/// can derive `token`/`token_known` from a `&Stoat`.
///
/// `None` when the focused pane is not an editor or its buffer is gone,
/// `Some(None)` when an index exists but no token covers the cursor, and
/// `Some(Some(kind))` when one does. The cursor offset resolves against the
/// buffer's own snapshot (a read lock) rather than the editor's display map,
/// which would need `&mut`.
pub(crate) fn cursor_token(ws: &Workspace) -> Option<Option<LspSymbolKind>> {
    let (editor, buffer_id) = resolve_focus(ws)?;
    let offset = {
        let buffer = ws.buffers.get(buffer_id)?;
        let guard = buffer.read().ok()?;
        let snapshot = &guard.snapshot;
        let sel = editor.selections.newest_anchor();
        stoat_text::cursor_offset(
            &snapshot.visible_text,
            snapshot.resolve_anchor(&sel.tail()),
            snapshot.resolve_anchor(&sel.head()),
        )
    };
    ws.buffers.lsp_symbol_kind_at(buffer_id, offset)
}

/// Resolve the focused split pane to its editor and buffer, the shared lookup
/// [`cursor_token`] and [`focus_flags`] both derive their fields from.
///
/// `None` when the focused pane is not an editor or its editor is gone.
fn resolve_focus(ws: &Workspace) -> Option<(&EditorState, BufferId)> {
    let View::Editor(editor_id) = ws.panes.pane(ws.panes.focus()).view else {
        return None;
    };
    let editor = ws.editors.get(editor_id)?;
    let buffer_id = editor.buffer_id;
    Some((editor, buffer_id))
}

/// The focused-buffer predicate values, all false or absent when no editor is
/// focused.
#[derive(Default, Hash)]
pub(crate) struct FocusFlags {
    lsp: bool,
    lang: Option<String>,
    diags: bool,
    has_selection: bool,
    modified: bool,
}

/// Derive [`FocusFlags`] for the focused buffer, sharing [`resolve_focus`] with
/// [`cursor_token`] so the focus lookup is written once.
pub(crate) fn focus_flags(
    ws: &Workspace,
    diagnostics: &DiagnosticSet,
    registry: &LspRegistry,
) -> FocusFlags {
    let Some((editor, buffer_id)) = resolve_focus(ws) else {
        return FocusFlags::default();
    };
    let lang = ws
        .buffers
        .language_for(buffer_id)
        .map(|l| l.name.to_string());
    let lsp = crate::lsp::session::lsp_language_name(&ws.buffers, buffer_id)
        .is_some_and(|name| registry.has_host_for_language(&name));
    let diags = ws
        .buffers
        .path_for(buffer_id)
        .is_some_and(|path| !diagnostics.get(path).is_empty());
    let has_selection = !editor.selections.newest_anchor().is_empty();
    let modified = ws
        .buffers
        .get(buffer_id)
        .and_then(|b| b.read().ok().map(|g| g.dirty))
        .unwrap_or(false);
    FocusFlags {
        lsp,
        lang,
        diags,
        has_selection,
        modified,
    }
}

/// The `View` of the active workspace's focused pane or dock.
///
/// The split-pane focus lives solely in [`crate::pane::PaneTree::focus`];
/// [`FocusTarget::SplitPane`] is a unit variant, so this resolves the focused
/// pane through the tree, staying in step with [`Stoat::focused_editor_ids`].
fn focused_view(ws: &Workspace) -> Option<&View> {
    match ws.focus {
        FocusTarget::SplitPane => Some(&ws.panes.pane(ws.panes.focus()).view),
        FocusTarget::Dock(dock_id) => Some(&ws.docks.get(dock_id)?.view),
    }
}

/// The focused pane's kind as a `pane` predicate value.
fn pane_predicate(ws: &Workspace) -> Option<&'static str> {
    Some(match focused_view(ws)? {
        View::Label(_) => "label",
        View::Editor(_) => "editor",
        View::Run(_) => "run",
        View::Agent(_) => "agent",
        View::Terminal(_) => "terminal",
        View::Image { .. } => "image",
    })
}

/// The active `view` predicate value, naming the app screen in the foreground.
///
/// App screens are not editor modes. They are derived from the session state
/// that already tracks them, resolved in the precedence order conflict > diff >
/// reword > rebase_conflict > rebase > commits > file so that a screen stacked
/// over another (a diff opened from the commit list, a reword paused
/// mid-rebase) reports the topmost one. `file` is any focused editor with no screen over it. The
/// value is absent when nothing is focused.
pub(crate) fn view_predicate(ws: &Workspace) -> Option<&'static str> {
    // The conflict resolve view is a swapped-in scratch editor with
    // `conflict_view` set rather than a pane variant of its own.
    if let Some(View::Editor(id)) = focused_view(ws)
        && ws
            .editors
            .get(*id)
            .is_some_and(|e| e.conflict_view.is_some())
    {
        return Some("conflict");
    }
    // The live per-file diff view is a normal editor with `diff_view` set. It
    // ranks below conflict, which swaps a whole editor in over it.
    if let Some(View::Editor(id)) = focused_view(ws)
        && ws.editors.get(*id).is_some_and(|e| e.diff_view)
    {
        return Some("diff");
    }
    match ws.rebase_active.as_ref().and_then(|a| a.pause.as_ref()) {
        Some(RebasePause::Reword { .. }) => return Some("reword"),
        Some(RebasePause::Conflict { .. }) => return Some("rebase_conflict"),
        // An Edit pause lands on the diff view, which the check above already
        // reports whenever a file was opened. This covers the commit that
        // changed nothing and so opened none, where the pause still has to keep
        // RebaseContinue bound.
        Some(RebasePause::Edit { .. }) => return Some("diff"),
        None => {},
    }
    if ws.rebase.is_some() {
        return Some("rebase");
    }
    if ws.commits.is_some() {
        return Some("commits");
    }
    if matches!(focused_view(ws), Some(View::Editor(_))) {
        return Some("file");
    }
    None
}

/// One of the modal surfaces that can own input, identified independently of the
/// `Option` field it happens to be stored in.
///
/// Stoat keeps each modal in its own field on [`Stoat`], so "which modal is
/// active" was answered separately, and differently, everywhere it was asked.
/// This enum plus [`active_modal`] is the single answer. A consumer that needs to
/// rank, name, or classify the active modal matches here instead of walking the
/// fields again in an order of its own.
///
/// Covers both the pickers/overlays and the transient text inputs (search,
/// shell, rename, ...). The latter are plain insert-mode editors, so the `modal`
/// predicate is the only thing distinguishing them from an ordinary buffer when
/// resolving keybindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveModal {
    Run,
    QuitConfirm,
    WorkspacePicker,
    Jumplist,
    Diagnostics,
    CommitPicker,
    Location,
    FileFinder,
    SymbolFinder,
    CodeSearch,
    Palette,
    Help,
    Rename,
    Search,
    SplitSelection,
    FilterSelections,
    ShellInput,
}

impl ActiveModal {
    /// The `modal` predicate value keybindings match on.
    ///
    /// The incremental `/` input is `isearch` rather than `search`, because a
    /// binding under it is scoped to the incremental walk, not to searching.
    pub(crate) fn context_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::QuitConfirm => "quit_confirm",
            Self::WorkspacePicker => "workspace_picker",
            Self::Jumplist => "jumplist",
            Self::Diagnostics => "diagnostics",
            Self::CommitPicker => "commit_picker",
            Self::Location => "location",
            Self::FileFinder => "finder",
            Self::SymbolFinder => "symbols",
            Self::CodeSearch => "code_search",
            Self::Palette => "palette",
            Self::Help => "help",
            Self::Rename => "rename",
            Self::Search => "isearch",
            Self::SplitSelection => "split_selection",
            Self::FilterSelections => "filter_selections",
            Self::ShellInput => "shell",
        }
    }

    /// Whether this modal's box covers the minimap band, so the strip is not
    /// worth drawing beneath it.
    ///
    /// False for the palette. Its box is capped at 80 columns and centered, so its
    /// right edge never passes `width / 2 + 40`, while the 8-column band exists
    /// only from `MINIMAP_MIN_PANE_COLS` up. The two rects are disjoint at every
    /// width where a band exists at all, so hiding the strip for the palette would
    /// cost the user their minimap for no overlap.
    ///
    /// Also false for the transient text inputs, which never claim the editor
    /// area as a box. Search draws in the focused pane's status bar, Rename
    /// draws a popup beside the cursor, and SplitSelection, FilterSelections
    /// and ShellInput do not paint yet.
    pub(crate) fn hides_minimap(self) -> bool {
        !matches!(
            self,
            Self::Palette
                | Self::Rename
                | Self::Search
                | Self::SplitSelection
                | Self::FilterSelections
                | Self::ShellInput
        )
    }

    /// Whether a zoom step belongs to this modal rather than to the panes behind
    /// it.
    ///
    /// Wider than [`Self::hides_minimap`] by the palette, which is left out there
    /// to keep the minimap visible beside it. That has nothing to do with input,
    /// and the palette takes zoom steps like any other modal.
    ///
    /// A modal with no [`ModalKind`] still holds here. Those size entirely to
    /// their content and have nothing to zoom, so a step over them does nothing.
    /// Resizing a pane hidden behind a modal would be a change the user cannot
    /// see or connect to what they pressed.
    pub(crate) fn owns_zoom(self) -> bool {
        self.hides_minimap() || self == Self::Palette
    }

    /// Whether the frame paints this modal as its own centered box.
    ///
    /// False for the transient text inputs. Search renders in the focused pane's
    /// status bar and Rename in the popup section beside the cursor, while
    /// SplitSelection, FilterSelections and ShellInput do not paint yet. All of
    /// them leave the frame free to paint the key-hints box over the editor
    /// behind them.
    pub(crate) fn paints_own_box(self) -> bool {
        !matches!(
            self,
            Self::Rename
                | Self::Search
                | Self::SplitSelection
                | Self::FilterSelections
                | Self::ShellInput
        )
    }

    /// The resizable-box kind a zoom step applies to, or `None` for a modal that
    /// sizes entirely to its content.
    pub(crate) fn zoom_kind(self) -> Option<ModalKind> {
        match self {
            Self::Help => Some(ModalKind::Help),
            Self::FileFinder => Some(ModalKind::FileFinder),
            Self::SymbolFinder => Some(ModalKind::SymbolFinder),
            Self::CodeSearch => Some(ModalKind::CodeSearch),
            Self::Palette => Some(ModalKind::Palette),
            Self::CommitPicker => Some(ModalKind::CommitPicker),
            _ => None,
        }
    }
}

/// The modal that owns input right now, or `None` when none is open.
///
/// The order below is canonical for the whole app because it is what keys already
/// route by. Anything that ranks modals differently would act on a modal the user
/// is not typing into. Every non-render consumer derives from this rather than
/// walking the fields itself.
///
/// Modals are mutually exclusive in practice, since the keymap's `!modal` guards
/// keep a modal's own context from reaching the bindings that open another. That
/// makes the order unobservable in normal operation and load-bearing only if some
/// path opens a second modal anyway, which
/// [`debug_assert_modal_exclusivity`] catches.
///
/// This resolver stays pure so a caller can ask what the canonical pick would be
/// over any state, including one with two modals open.
/// Dismiss the modal that currently owns key input, reporting whether one was
/// open.
///
/// Matching on [`active_modal`] is what keeps the close order and the key
/// order from drifting apart, so the modal dismissed is always the one the
/// user is typing into. A modal owning a scratch editor is disposed rather
/// than dropped, or its input stays in the workspace for the rest of the
/// session.
///
/// A run pane is the one arm that does not close. It owns the whole pane
/// rather than floating over it, so it interrupts its shell and stays open.
pub(crate) fn close_topmost_modal(stoat: &mut Stoat) -> bool {
    let Some(modal) = active_modal(stoat) else {
        return false;
    };

    match modal {
        ActiveModal::Run => {
            let run_id = stoat.modal_run.expect("the run modal is open");
            let ws = stoat.active_workspace_mut();
            if let Some(run_state) = ws.runs.get_mut(run_id) {
                if let Some(handle) = &mut run_state.shell_handle {
                    handle.kill();
                }
                if let Some(block) = run_state.active_block_mut() {
                    block.finished = true;
                }
            }
        },
        ActiveModal::QuitConfirm => stoat.quit_all_confirm = None,
        ActiveModal::WorkspacePicker => {
            if let Some(picker) = stoat.workspace_picker.take() {
                picker.dispose(stoat.active_workspace_mut());
            }
        },
        ActiveModal::Jumplist => {
            if let Some(picker) = stoat.jumplist_picker.take() {
                picker.dispose(stoat.active_workspace_mut());
            }
        },
        ActiveModal::Diagnostics => {
            if let Some(picker) = stoat.diagnostics_picker.take() {
                picker.dispose(stoat.active_workspace_mut());
            }
        },
        ActiveModal::CommitPicker => {
            action_handlers::review_walk::commit_picker_close(stoat);
        },
        ActiveModal::Location => {
            stoat.location_picker = None;
        },
        ActiveModal::FileFinder => action_handlers::close_file_finder(stoat),
        ActiveModal::SymbolFinder => crate::symbol_finder::close_symbol_finder(stoat),
        ActiveModal::CodeSearch => {
            action_handlers::code_search::close_code_search(stoat);
        },
        ActiveModal::Palette => {
            if let Some(palette) = stoat.command_palette.take() {
                let active_idx = stoat.active_workspace;
                palette.dispose(&mut stoat.workspaces[active_idx]);
            }
        },
        ActiveModal::Help => action_handlers::help::close_help(stoat),
        ActiveModal::Rename => {
            action_handlers::lsp::rename_input_cancel(stoat);
        },
        ActiveModal::Search => {
            action_handlers::search::search_cancel(stoat);
        },
        ActiveModal::SplitSelection => {
            action_handlers::split_selection::cancel(stoat);
        },
        ActiveModal::FilterSelections => {
            action_handlers::filter_selections::cancel(stoat);
        },
        ActiveModal::ShellInput => {
            action_handlers::shell::cancel(stoat);
        },
    }

    true
}

pub(crate) fn active_modal(stoat: &Stoat) -> Option<ActiveModal> {
    if stoat.modal_run.is_some() {
        Some(ActiveModal::Run)
    } else if stoat.quit_all_confirm.is_some() {
        Some(ActiveModal::QuitConfirm)
    } else if stoat.workspace_picker.is_some() {
        Some(ActiveModal::WorkspacePicker)
    } else if stoat.jumplist_picker.is_some() {
        Some(ActiveModal::Jumplist)
    } else if stoat.diagnostics_picker.is_some() {
        Some(ActiveModal::Diagnostics)
    } else if stoat.commit_picker.is_some() {
        Some(ActiveModal::CommitPicker)
    } else if stoat.location_picker.is_some() {
        Some(ActiveModal::Location)
    } else if stoat.file_finder.is_some() {
        Some(ActiveModal::FileFinder)
    } else if stoat.symbol_finder.is_some() {
        Some(ActiveModal::SymbolFinder)
    } else if stoat.code_search.is_some() {
        Some(ActiveModal::CodeSearch)
    } else if stoat.command_palette.is_some() {
        Some(ActiveModal::Palette)
    } else if stoat.help.is_some() {
        Some(ActiveModal::Help)
    } else if stoat.rename_input.is_some() {
        Some(ActiveModal::Rename)
    } else if stoat.search_input.is_some() {
        Some(ActiveModal::Search)
    } else if stoat.split_selection_input.is_some() {
        Some(ActiveModal::SplitSelection)
    } else if stoat.filter_selections_input.is_some() {
        Some(ActiveModal::FilterSelections)
    } else if stoat.shell_input.is_some() {
        Some(ActiveModal::ShellInput)
    } else {
        None
    }
}

/// Panic in a debug build when two modals are open at once.
///
/// The keymap's `!modal` guards are what hold this invariant, since a modal's own
/// context never reaches the bindings that would open another. Breaking it is a
/// routing bug rather than a rendering one, because keys and paint then disagree
/// about which modal the user is in, so the check sits on the key path and runs
/// once per press rather than once per render.
///
/// Debug-only because the release behavior wanted here is
/// [`active_modal`]'s canonical pick, not a panic in the user's editor.
pub(crate) fn debug_assert_modal_exclusivity(stoat: &Stoat) {
    debug_assert!(
        open_modal_count(stoat) <= 1,
        "modals must be mutually exclusive, found {} open",
        open_modal_count(stoat)
    );
}

fn open_modal_count(stoat: &Stoat) -> usize {
    [
        stoat.modal_run.is_some(),
        stoat.quit_all_confirm.is_some(),
        stoat.workspace_picker.is_some(),
        stoat.jumplist_picker.is_some(),
        stoat.diagnostics_picker.is_some(),
        stoat.commit_picker.is_some(),
        stoat.location_picker.is_some(),
        stoat.file_finder.is_some(),
        stoat.symbol_finder.is_some(),
        stoat.code_search.is_some(),
        stoat.command_palette.is_some(),
        stoat.help.is_some(),
        stoat.rename_input.is_some(),
        stoat.search_input.is_some(),
        stoat.split_selection_input.is_some(),
        stoat.filter_selections_input.is_some(),
        stoat.shell_input.is_some(),
    ]
    .iter()
    .filter(|open| **open)
    .count()
}

/// The topmost open modal as a `modal` predicate value, in canonical precedence.
/// Absent when no modal is open.
pub(crate) fn modal_predicate(stoat: &Stoat) -> Option<&'static str> {
    active_modal(stoat).map(ActiveModal::context_str)
}

/// Strip the `SHIFT` modifier from events where it duplicates information
/// already carried by the keycode, so bindings written without an explicit
/// `S-` prefix still match what the terminal emits.
///
/// Default crossterm without the kitty keyboard protocol reports Shift+a as
/// `(Char('A'), SHIFT)` and Shift-Tab (CSI Z) as `(BackTab, SHIFT)`, but
/// bindings written as `A` or `BackTab` compile to `(_, NONE)` and modifier
/// comparison in [`crate::keymap::CompiledKey::matches`] is strict. For
/// `Char(letter)` the uppercase code already encodes Shift; for `BackTab`
/// the keycode itself is the Shift-Tab variant. In both cases the SHIFT
/// modifier is redundant, so dropping it up-front keeps bindings
/// terminal-agnostic.
pub(crate) fn normalize_shift_event(key: KeyEvent) -> KeyEvent {
    if !key.modifiers.contains(KeyModifiers::SHIFT) {
        return key;
    }
    let new_code = match key.code {
        KeyCode::Char(ch) if ch.is_ascii_alphabetic() => KeyCode::Char(ch.to_ascii_uppercase()),
        KeyCode::BackTab => KeyCode::BackTab,
        _ => return key,
    };
    let mut modifiers = key.modifiers;
    modifiers.remove(KeyModifiers::SHIFT);
    KeyEvent {
        code: new_code,
        modifiers,
        ..key
    }
}

pub(crate) fn arg_as_str(arg: &ResolvedArg) -> Option<String> {
    match &arg.value {
        stoat_config::Value::String(s) => Some(s.clone()),
        stoat_config::Value::Ident(s) => Some(s.clone()),
        _ => None,
    }
}

/// The [`StateValue`] a `SetVar` value argument sets, mapping a string/ident to
/// a string, a number to a number, and a bool to a bool. `None` for a value
/// shape a predicate cannot compare against.
pub(crate) fn arg_to_state_value(arg: &ResolvedArg) -> Option<StateValue> {
    match &arg.value {
        stoat_config::Value::String(s) | stoat_config::Value::Ident(s) => {
            Some(StateValue::String(s.as_str().into()))
        },
        stoat_config::Value::Number(n) => Some(StateValue::Number(*n)),
        stoat_config::Value::Bool(b) => Some(StateValue::Bool(*b)),
        _ => None,
    }
}

fn arg_to_param_value(
    arg: &ResolvedArg,
    captured: Option<f64>,
) -> Option<stoat_action::ParamValue> {
    match &arg.value {
        stoat_config::Value::String(s) => Some(stoat_action::ParamValue::String(s.clone())),
        stoat_config::Value::Ident(s) => Some(stoat_action::ParamValue::String(s.clone())),
        stoat_config::Value::Number(n) => Some(stoat_action::ParamValue::Number(*n)),
        stoat_config::Value::Bool(b) => Some(stoat_action::ParamValue::Bool(*b)),
        stoat_config::Value::StateRef(name) if name == "num" => {
            captured.map(stoat_action::ParamValue::Number)
        },
        _ => None,
    }
}

pub(crate) fn action_display_desc(action: &ResolvedAction) -> String {
    if action.name == "SetMode" {
        let target = action.args.first().and_then(arg_as_str).unwrap_or_default();
        return format!("{target} mode");
    }
    if action.name == "SetVar" {
        let name = action.args.first().and_then(arg_as_str).unwrap_or_default();
        return format!("set {name}");
    }
    stoat_action::registry::lookup(&action.name)
        .map(|e| e.def.short_desc().to_string())
        .unwrap_or_else(|| action.name.clone())
}

/// The hint label for a whole binding, naming what the key actually does.
///
/// A chord binding leads with `SetMode(normal)` so the origin editor's mode
/// resets before the action runs, which means the first action describes the
/// mode reset rather than the key's purpose. The label therefore names the
/// first action that is not a mode switch, and falls back to the first action
/// for a binding that only switches modes.
pub(crate) fn binding_display_desc(actions: &[ResolvedAction]) -> String {
    actions
        .iter()
        .find(|action| action.name != "SetMode")
        .or_else(|| actions.first())
        .map(action_display_desc)
        .unwrap_or_default()
}

/// Build the action `name` with `args`, or `None` if unknown or an argument
/// cannot be converted.
///
/// `captured` is the digit a `num` placeholder key matched this press, the value
/// a `$num` argument resolves to. It is `None` for every non-placeholder
/// binding, so a `$num` argument there has no value and drops the action.
pub(crate) fn resolve_action(
    name: &str,
    args: &[ResolvedArg],
    captured: Option<f64>,
) -> Option<Box<dyn Action>> {
    let entry = stoat_action::registry::lookup(name)?;
    let mut params = Vec::with_capacity(args.len());
    for arg in args {
        match arg_to_param_value(arg, captured) {
            Some(value) => params.push(value),
            None => {
                tracing::warn!("action `{name}`: cannot convert arg {:?}", arg.value);
                return None;
            },
        }
    }
    match (entry.create)(&params) {
        Ok(action) => Some(action),
        Err(e) => {
            tracing::warn!("action `{name}`: {e}");
            None
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::RunId;
    use stoat_config::Value;

    fn field(state: &StoatKeymapState<'_>, name: &str) -> Option<String> {
        match state.get(name) {
            Some(StateValue::String(s)) => Some(s.to_string()),
            _ => None,
        }
    }

    #[test]
    fn from_stoat_default_is_editor_file_no_modal() {
        let h = Stoat::test();
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(field(&state, "pane"), Some("editor".to_string()));
        assert_eq!(field(&state, "view"), Some("file".to_string()));
        assert_eq!(state.get("modal"), None);
    }

    #[test]
    fn from_stoat_run_pane_is_run() {
        let mut h = Stoat::test();
        {
            let ws = h.stoat.active_workspace_mut();
            if matches!(ws.focus, FocusTarget::SplitPane) {
                let pane_id = ws.panes.focus();
                ws.panes.pane_mut(pane_id).view = View::Run(RunId::default());
            }
        }
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(field(&state, "pane"), Some("run".to_string()));
        assert_eq!(state.get("view"), None);
    }

    fn open_foo_bar(h: &mut crate::test_harness::TestHarness) -> BufferId {
        let root = std::path::PathBuf::from("/lsp");
        let path = root.join("a.rs");
        h.fake_fs().insert_file(&path, b"Foo bar");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFile { path });
        h.settle();
        let ws = h.stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => ws.editors[id].buffer_id,
            _ => panic!("focused pane is not an editor"),
        }
    }

    /// Seed a symbol-kind index over "Foo bar": `Foo` [0,3) a trait, `bar` [4,7)
    /// a function, leaving the space at offset 3 uncovered.
    fn seed_foo_bar_kinds(h: &mut crate::test_harness::TestHarness, id: BufferId) {
        let ws = h.stoat.active_workspace_mut();
        let snapshot = ws
            .buffers
            .get(id)
            .expect("buffer")
            .read()
            .unwrap()
            .snapshot
            .clone();
        let start = |o| snapshot.anchors_at_batch(&[o], stoat_text::Bias::Right)[0];
        let end = |o| snapshot.anchors_at_batch(&[o], stoat_text::Bias::Left)[0];
        let kinds = std::sync::Arc::from(vec![
            (start(0usize)..end(3usize), LspSymbolKind::Trait),
            (start(4usize)..end(7usize), LspSymbolKind::Function),
        ]);
        ws.buffers.store_lsp_symbol_kinds(id, kinds);
    }

    #[test]
    fn from_stoat_token_absent_without_an_index() {
        let mut h = Stoat::test();
        open_foo_bar(&mut h);
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(state.get("token_known"), Some(&StateValue::Bool(false)));
        assert_eq!(state.get("token"), None);
    }

    #[test]
    fn from_stoat_token_known_but_absent_on_plain_text() {
        let mut h = Stoat::test();
        let id = open_foo_bar(&mut h);
        seed_foo_bar_kinds(&mut h, id);
        action_handlers::movement::jump_to_offset(&mut h.stoat, 3);
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(state.get("token_known"), Some(&StateValue::Bool(true)));
        assert_eq!(
            state.get("token"),
            None,
            "the space between tokens is untyped"
        );
    }

    #[test]
    fn from_stoat_token_is_the_kind_under_the_cursor() {
        let mut h = Stoat::test();
        let id = open_foo_bar(&mut h);
        seed_foo_bar_kinds(&mut h, id);
        action_handlers::movement::jump_to_offset(&mut h.stoat, 4);
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(state.get("token_known"), Some(&StateValue::Bool(true)));
        assert_eq!(field(&state, "token"), Some("function".to_string()));
    }

    #[test]
    fn from_stoat_lsp_lang_and_diags_for_a_rust_buffer() {
        use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
        let mut h = Stoat::test();
        open_foo_bar(&mut h);
        h.seed_diagnostics(
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

        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(field(&state, "lang"), Some("rust".to_string()));
        assert_eq!(
            state.get("lsp"),
            Some(&StateValue::Bool(true)),
            "the fake client serves rust",
        );
        assert_eq!(state.get("diags"), Some(&StateValue::Bool(true)));
    }

    #[test]
    fn from_stoat_modified_true_after_an_edit() {
        let mut h = Stoat::test();
        open_foo_bar(&mut h);
        h.type_keys("i x escape");
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(state.get("modified"), Some(&StateValue::Bool(true)));
    }

    #[test]
    fn from_stoat_has_selection_true_with_an_active_selection() {
        let mut h = Stoat::test();
        open_foo_bar(&mut h);
        h.type_keys("v l");
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(state.get("has_selection"), Some(&StateValue::Bool(true)));
    }

    #[test]
    fn from_stoat_lang_absent_for_a_scratch_buffer() {
        let h = Stoat::test();
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(state.get("lang"), None, "a scratch buffer has no grammar");
        assert_eq!(state.get("lsp"), Some(&StateValue::Bool(false)));
    }

    #[test]
    fn diff_action_toggles_the_diff_view() {
        let mut h = Stoat::test();
        assert_eq!(
            h.stoat.current_view(),
            Some("file"),
            "a plain editor starts outside the diff view"
        );
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Diff { rev: None });
        assert_eq!(
            h.stoat.current_view(),
            Some("diff"),
            "Diff turns the diff view on"
        );
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Diff { rev: None });
        assert_eq!(
            h.stoat.current_view(),
            Some("file"),
            "Diff again turns it back off"
        );
    }

    #[test]
    fn escape_in_the_diff_view_stays_in_the_view() {
        let mut h = Stoat::test();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Diff { rev: None });
        assert_eq!(h.stoat.current_view(), Some("diff"));

        h.type_keys("Escape");

        assert_eq!(
            h.stoat.current_view(),
            Some("diff"),
            "Escape does not leave the diff view"
        );
    }

    #[test]
    fn from_stoat_commits_is_commits_view() {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("c1", "init", &[("a.rs", "fn a() {}\n")]),
                ("c2", "more", &[("a.rs", "fn a() {}\nfn b() {}\n")]),
            ],
        );
        h.open_commits("/repo");
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(field(&state, "view"), Some("commits".to_string()));
        assert_eq!(
            field(&state, "mode"),
            Some("normal".to_string()),
            "the commits screen is a view, so the editor stays in normal mode"
        );
    }

    #[test]
    fn from_stoat_modal_maps_topmost() {
        let mut h = Stoat::test();
        assert_eq!(StoatKeymapState::from_stoat(&h.stoat).get("modal"), None);

        h.stoat.modal_run = Some(RunId::default());
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(field(&state, "modal"), Some("run".to_string()));
    }

    #[test]
    fn from_stoat_modal_covers_text_inputs() {
        let mut h = Stoat::test();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSearchInput);
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(field(&state, "modal"), Some("isearch".to_string()));
    }

    /// Every variant, in the canonical order [`active_modal`] resolves them.
    const ALL_MODALS: [ActiveModal; 17] = [
        ActiveModal::Run,
        ActiveModal::QuitConfirm,
        ActiveModal::WorkspacePicker,
        ActiveModal::Jumplist,
        ActiveModal::Diagnostics,
        ActiveModal::CommitPicker,
        ActiveModal::Location,
        ActiveModal::FileFinder,
        ActiveModal::SymbolFinder,
        ActiveModal::CodeSearch,
        ActiveModal::Palette,
        ActiveModal::Help,
        ActiveModal::Rename,
        ActiveModal::Search,
        ActiveModal::SplitSelection,
        ActiveModal::FilterSelections,
        ActiveModal::ShellInput,
    ];

    fn context_strs(modals: impl IntoIterator<Item = ActiveModal>) -> Vec<&'static str> {
        modals.into_iter().map(ActiveModal::context_str).collect()
    }

    /// Config keybindings match these strings, so a renamed one silently unbinds
    /// every key scoped to that modal.
    #[test]
    fn every_modal_keeps_its_predicate_string() {
        assert_eq!(
            context_strs(ALL_MODALS),
            [
                "run",
                "quit_confirm",
                "workspace_picker",
                "jumplist",
                "diagnostics",
                "commit_picker",
                "location",
                "finder",
                "symbols",
                "code_search",
                "palette",
                "help",
                "rename",
                "isearch",
                "split_selection",
                "filter_selections",
                "shell",
            ]
        );
    }

    #[test]
    fn the_palette_keeps_the_minimap_but_still_takes_zoom_steps() {
        assert_eq!(
            context_strs(ALL_MODALS.into_iter().filter(|m| m.hides_minimap())),
            [
                "run",
                "quit_confirm",
                "workspace_picker",
                "jumplist",
                "diagnostics",
                "commit_picker",
                "location",
                "finder",
                "symbols",
                "code_search",
                "help",
            ],
            "the palette and the transient inputs leave the strip alone"
        );
        assert_eq!(
            context_strs(ALL_MODALS.into_iter().filter(|m| m.owns_zoom())),
            [
                "run",
                "quit_confirm",
                "workspace_picker",
                "jumplist",
                "diagnostics",
                "commit_picker",
                "location",
                "finder",
                "symbols",
                "code_search",
                "palette",
                "help",
            ],
            "zoom adds the palette and nothing else"
        );
    }

    #[test]
    fn only_resizable_modals_carry_a_zoom_kind() {
        assert_eq!(
            context_strs(ALL_MODALS.into_iter().filter(|m| m.zoom_kind().is_some())),
            [
                "commit_picker",
                "finder",
                "symbols",
                "code_search",
                "palette",
                "help"
            ]
        );
    }

    /// The canonical order is only observable when two modals are somehow open at
    /// once, which the keymap's `!modal` guards prevent. This pins what the app
    /// does anyway if some path ever opens a second one.
    #[test]
    fn a_quit_prompt_outranks_a_finder_opened_under_it() {
        let mut h = Stoat::test();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
        assert_eq!(
            active_modal(&h.stoat),
            Some(ActiveModal::FileFinder),
            "the finder alone is the active modal"
        );

        h.stoat.quit_all_confirm = Some(crate::quit_all_confirm::QuitAllConfirm::new(
            &[],
            std::path::Path::new("/"),
        ));

        assert_eq!(
            active_modal(&h.stoat),
            Some(ActiveModal::QuitConfirm),
            "the prompt keys route to wins over the finder behind it"
        );
    }

    #[test]
    fn user_var_reads_through_get_without_shadowing_builtins() {
        let mut h = Stoat::test();
        h.stoat
            .user_vars
            .insert("sidebar".into(), StateValue::String("on".into()));
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(field(&state, "sidebar"), Some("on".to_string()));
        assert_eq!(field(&state, "mode"), Some("normal".to_string()));
    }

    fn resolved(name: &str, args: Vec<Value>) -> ResolvedAction {
        ResolvedAction {
            name: name.to_string(),
            args: args
                .into_iter()
                .map(|value| ResolvedArg { name: None, value })
                .collect(),
        }
    }

    fn set_mode(target: &str) -> ResolvedAction {
        resolved("SetMode", vec![Value::Ident(target.to_string())])
    }

    #[test]
    fn binding_desc_names_the_action_behind_a_mode_reset() {
        let actions = [set_mode("normal"), resolved("PrevTab", vec![])];
        assert_eq!(
            binding_display_desc(&actions),
            "switch to the previous tab",
            "the leading mode reset is not what the key does"
        );
    }

    #[test]
    fn binding_desc_names_an_action_with_args_behind_a_mode_reset() {
        let actions = [
            set_mode("normal"),
            resolved("GotoTab", vec![Value::Number(3.0)]),
        ];
        assert_eq!(binding_display_desc(&actions), "switch to tab by number");
    }

    #[test]
    fn binding_desc_of_a_bare_mode_switch_names_the_mode() {
        assert_eq!(binding_display_desc(&[set_mode("normal")]), "normal mode");
    }

    #[test]
    fn binding_desc_of_a_bare_prefix_switch_names_the_prefix_mode() {
        assert_eq!(
            binding_display_desc(&[set_mode("prefix_tab")]),
            "prefix_tab mode"
        );
    }
}
