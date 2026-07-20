use crate::{
    app::Stoat,
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
#[derive(Default)]
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
#[derive(Default)]
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
    let lsp = crate::action_handlers::lsp::lsp_language_name(&ws.buffers, buffer_id)
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
    })
}

/// The active `view` predicate value, naming the app screen in the foreground.
///
/// App screens are not editor modes. They are derived from the session state
/// that already tracks them, resolved in the precedence order conflict > diff >
/// reword > rebase_conflict > rebase > commits > file so that a screen stacked over another (a
/// diff opened from the commit list, a reword paused mid-rebase) reports the
/// topmost one. `file` is any focused editor with no screen over it. The value
/// is absent when nothing is focused.
pub(crate) fn view_predicate(ws: &Workspace) -> Option<&'static str> {
    if let Some(View::Editor(id)) = focused_view(ws)
        && ws.editors.get(*id).is_some_and(|e| e.review_view.is_some())
    {
        return Some("review");
    }
    // The conflict resolve view is a swapped-in scratch editor with
    // `conflict_view` set, checked after the review session for the same reason.
    if let Some(View::Editor(id)) = focused_view(ws)
        && ws.editors.get(*id).is_some_and(|e| e.conflict_view)
    {
        return Some("conflict");
    }
    // The live per-file diff view is a normal editor with `diff_view` set. It is
    // checked after the session arm so a session screen keeps precedence.
    if let Some(View::Editor(id)) = focused_view(ws)
        && ws.editors.get(*id).is_some_and(|e| e.diff_view)
    {
        return Some("diff");
    }
    match ws.rebase_active.as_ref().and_then(|a| a.pause.as_ref()) {
        Some(RebasePause::Reword { .. }) => return Some("reword"),
        Some(RebasePause::Conflict { .. }) => return Some("rebase_conflict"),
        // An Edit pause reviews the picked commit. It normally installs a review
        // session (caught by the `review` check above), but the no-session
        // fallback still needs the review screen so RebaseContinue stays bound.
        Some(RebasePause::Edit { .. }) => return Some("review"),
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

/// The topmost open modal as a `modal` predicate value, in render precedence.
/// Absent when no modal is open.
///
/// Covers both the pickers/overlays and the transient text inputs (search,
/// shell, rename, ...). The latter are plain insert-mode editors, so the
/// `modal` value is the only thing distinguishing them from an ordinary buffer
/// when resolving keybindings. `search` is the global-search *results* picker;
/// the incremental `/` input is `isearch`, its query counterpart
/// `global_search`.
pub(crate) fn modal_predicate(stoat: &Stoat) -> Option<&'static str> {
    if stoat.modal_run.is_some() {
        Some("run")
    } else if stoat.quit_all_confirm.is_some() {
        Some("quit_confirm")
    } else if stoat.workspace_picker.is_some() {
        Some("workspace_picker")
    } else if stoat.jumplist_picker.is_some() {
        Some("jumplist")
    } else if stoat.diagnostics_picker.is_some() {
        Some("diagnostics")
    } else if stoat.location_picker.is_some() {
        Some("location")
    } else if stoat.global_search.is_some() {
        Some("search")
    } else if stoat.file_finder.is_some() {
        Some("finder")
    } else if stoat.symbol_finder.is_some() {
        Some("symbols")
    } else if stoat.code_search.is_some() {
        Some("code_search")
    } else if stoat.command_palette.is_some() {
        Some("palette")
    } else if stoat.help.is_some() {
        Some("help")
    } else if stoat.rename_input.is_some() {
        Some("rename")
    } else if stoat.search_input.is_some() {
        Some("isearch")
    } else if stoat.global_search_input.is_some() {
        Some("global_search")
    } else if stoat.split_selection_input.is_some() {
        Some("split_selection")
    } else if stoat.filter_selections_input.is_some() {
        Some("filter_selections")
    } else if stoat.shell_input.is_some() {
        Some("shell")
    } else {
        None
    }
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

fn arg_to_param_value(arg: &ResolvedArg) -> Option<stoat_action::ParamValue> {
    match &arg.value {
        stoat_config::Value::String(s) => Some(stoat_action::ParamValue::String(s.clone())),
        stoat_config::Value::Ident(s) => Some(stoat_action::ParamValue::String(s.clone())),
        stoat_config::Value::Number(n) => Some(stoat_action::ParamValue::Number(*n)),
        stoat_config::Value::Bool(b) => Some(stoat_action::ParamValue::Bool(*b)),
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

pub(crate) fn resolve_action(name: &str, args: &[ResolvedArg]) -> Option<Box<dyn Action>> {
    let entry = stoat_action::registry::lookup(name)?;
    let mut params = Vec::with_capacity(args.len());
    for arg in args {
        match arg_to_param_value(arg) {
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
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFile { path });
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
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 3);
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
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 4);
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(state.get("token_known"), Some(&StateValue::Bool(true)));
        assert_eq!(field(&state, "token"), Some("function".to_string()));
    }

    #[test]
    fn from_stoat_lsp_lang_and_diags_for_a_rust_buffer() {
        use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
        let mut h = Stoat::test();
        open_foo_bar(&mut h);
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
    fn from_stoat_review_is_diff_view() {
        let mut h = Stoat::test();
        h.open_review_from_texts(&[("a.rs", "fn a() {}\n", "fn b() {}\n")]);
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(field(&state, "pane"), Some("editor".to_string()));
        assert_eq!(field(&state, "view"), Some("review".to_string()));
    }

    #[test]
    fn diff_action_toggles_the_diff_view() {
        let mut h = Stoat::test();
        assert_eq!(
            h.stoat.current_view(),
            Some("file"),
            "a plain editor starts outside the diff view"
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Diff);
        assert_eq!(
            h.stoat.current_view(),
            Some("diff"),
            "Diff turns the diff view on"
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Diff);
        assert_eq!(
            h.stoat.current_view(),
            Some("file"),
            "Diff again turns it back off"
        );
    }

    #[test]
    fn escape_in_the_diff_view_stays_in_the_view() {
        let mut h = Stoat::test();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Diff);
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
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSearchInput);
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(field(&state, "modal"), Some("isearch".to_string()));
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
}
