use crate::{
    app::Stoat,
    keymap::{KeymapState, ResolvedAction, ResolvedArg, StateValue},
    pane::{FocusTarget, View},
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
    "palette_open",
    "help_open",
    "finder_open",
    "rebase_exec",
];

/// The still-hand-set modal booleans a keymap state carries besides `mode`.
///
/// Passed to [`StoatKeymapState::with_flags`] so a caller sets only the flags it
/// needs and the derived `pane`/`view`/`modal` predicates do not ripple through
/// its signature. Retired incrementally by the keymap rework's cleanup step.
#[derive(Default)]
pub(crate) struct Flags {
    pub(crate) palette_open: bool,
    pub(crate) help_open: bool,
    pub(crate) finder_open: bool,
    pub(crate) rebase_exec: bool,
}

pub(crate) struct StoatKeymapState {
    mode_value: StateValue,
    palette_open: StateValue,
    help_open: StateValue,
    finder_open: StateValue,
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
    /// Config-defined session variables, read only after the built-in fields so
    /// a variable can never shadow one.
    user_vars: HashMap<String, StateValue>,
}

impl StoatKeymapState {
    #[cfg(test)]
    pub(crate) fn new(mode: &str) -> Self {
        Self::with_flags(mode, Flags::default())
    }

    pub(crate) fn with_flags(mode: &str, flags: Flags) -> Self {
        Self {
            mode_value: StateValue::String(mode.into()),
            palette_open: StateValue::Bool(flags.palette_open),
            help_open: StateValue::Bool(flags.help_open),
            finder_open: StateValue::Bool(flags.finder_open),
            rebase_exec: StateValue::Bool(flags.rebase_exec),
            pane: None,
            view: None,
            modal: None,
            user_vars: HashMap::new(),
        }
    }

    pub(crate) fn from_stoat(stoat: &Stoat) -> Self {
        let ws = stoat.active_workspace();
        let flags = Flags {
            palette_open: stoat.command_palette.is_some(),
            help_open: stoat.help.is_some(),
            finder_open: stoat.file_finder.is_some(),
            rebase_exec: ws.rebase_active.is_some(),
        };
        Self {
            pane: pane_predicate(ws).map(|s| StateValue::String(s.into())),
            view: view_predicate(ws).map(|s| StateValue::String(s.into())),
            modal: modal_predicate(stoat).map(|s| StateValue::String(s.into())),
            user_vars: stoat.user_vars.clone(),
            ..Self::with_flags(stoat.focused_mode(), flags)
        }
    }
}

impl KeymapState for StoatKeymapState {
    fn get(&self, field: &str) -> Option<&StateValue> {
        match field {
            "mode" => Some(&self.mode_value),
            "palette_open" => Some(&self.palette_open),
            "help_open" => Some(&self.help_open),
            "finder_open" => Some(&self.finder_open),
            "rebase_exec" => Some(&self.rebase_exec),
            "pane" => self.pane.as_ref(),
            "view" => self.view.as_ref(),
            "modal" => self.modal.as_ref(),
            other => self.user_vars.get(other),
        }
    }
}

/// The `View` of the active workspace's focused pane or dock.
fn focused_view(ws: &Workspace) -> Option<&View> {
    match ws.focus {
        FocusTarget::SplitPane(pane_id) => Some(&ws.panes.pane(pane_id).view),
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

/// The focused editor's `view` predicate value, `diff` under a review and `file`
/// otherwise. Absent unless an editor is focused.
fn view_predicate(ws: &Workspace) -> Option<&'static str> {
    match focused_view(ws)? {
        View::Editor(id) => Some(
            if ws.editors.get(*id).is_some_and(|e| e.review_view.is_some()) {
                "diff"
            } else {
                "file"
            },
        ),
        _ => None,
    }
}

/// The topmost open modal as a `modal` predicate value, in render precedence.
/// Absent when no modal is open.
fn modal_predicate(stoat: &Stoat) -> Option<&'static str> {
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
    } else if stoat.command_palette.is_some() {
        Some("palette")
    } else if stoat.help.is_some() {
        Some("help")
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

    fn field(state: &StoatKeymapState, name: &str) -> Option<String> {
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
            if let FocusTarget::SplitPane(pane_id) = ws.focus {
                ws.panes.pane_mut(pane_id).view = View::Run(RunId::default());
            }
        }
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(field(&state, "pane"), Some("run".to_string()));
        assert_eq!(state.get("view"), None);
    }

    #[test]
    fn from_stoat_review_is_diff_view() {
        let mut h = Stoat::test();
        h.open_review_from_texts(&[("a.rs", "fn a() {}\n", "fn b() {}\n")]);
        let state = StoatKeymapState::from_stoat(&h.stoat);
        assert_eq!(field(&state, "pane"), Some("editor".to_string()));
        assert_eq!(field(&state, "view"), Some("diff".to_string()));
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
