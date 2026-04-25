use crate::{
    input_view::{InputView, SubmitTarget},
    keymap::{ResolvedAction, ResolvedArg},
    workspace::Workspace,
};
use stoat_action::{registry, ActionDef, ActionKind, ParamDef, ParamValue};
use stoat_config::Value;
use stoat_scheduler::Executor;

/// Synthetic [`ActionDef`] used to surface `SetMode` keybindings in help.
/// `SetMode` is handled specially in [`crate::app::Stoat::handle_key`] and
/// never reaches the action registry, so it needs a static def purely for
/// display.
#[derive(Debug)]
struct SetModeHelpDef;

impl ActionDef for SetModeHelpDef {
    fn name(&self) -> &'static str {
        "SetMode"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenHelp
    }

    fn params(&self) -> &'static [ParamDef] {
        SET_MODE_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "transition to a named mode"
    }

    fn long_desc(&self) -> &'static str {
        "Enter the named mode. Modes gate which keybindings are active: `normal` \
         is the base navigation mode, `insert` accepts raw character input, and \
         leader-style submodes like `space` chain into further bindings. The \
         target is the first argument."
    }

    fn palette_visible(&self) -> bool {
        false
    }
}

static SET_MODE_DEF: SetModeHelpDef = SetModeHelpDef;
static SET_MODE_PARAMS: &[ParamDef] = &[ParamDef {
    name: "mode",
    kind: stoat_action::ParamKind::String,
    required: true,
    description: "Name of the target mode (e.g. `normal`, `insert`, `space`).",
}];

pub struct Help {
    pub(crate) input: InputView,
    scope: HelpScope,
    /// Mode to restore when the help modal closes. Saved at `new()` time so
    /// `close_help` in the action-handler layer can transition
    /// [`crate::app::Stoat::mode`] back to whatever the user was in before
    /// `?` was pressed.
    pub(crate) previous_mode: String,
    snapshot_mode: String,
    active: Vec<(String, Vec<ResolvedAction>)>,
    entries: Vec<HelpEntry>,
    filtered: Vec<usize>,
    selected: usize,
    detail_scroll: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpInput {
    Insert,
    Normal,
}

/// Derive the help "sub-mode" from the global [`crate::app::Stoat::mode`].
/// `"prompt"` maps to [`HelpInput::Insert`] (user is typing into the search
/// field); anything else maps to [`HelpInput::Normal`] (user is navigating
/// the list with hjkl).
pub fn help_input_mode(stoat_mode: &str) -> HelpInput {
    match stoat_mode {
        "prompt" => HelpInput::Insert,
        _ => HelpInput::Normal,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpScope {
    Active,
    All,
}

pub struct HelpEntry {
    pub def: &'static dyn ActionDef,
    pub key_label: Option<String>,
    pub bound_args: Vec<ResolvedArg>,
}

pub enum HelpOutcome {
    None,
    Close,
    Dispatch(&'static registry::RegistryEntry, Vec<ParamValue>),
}

impl Help {
    pub fn new(
        snapshot_mode: &str,
        active: Vec<(String, Vec<ResolvedAction>)>,
        ws: &mut Workspace,
        executor: Executor,
        previous_mode: String,
    ) -> Self {
        let input = InputView::create(ws, executor, SubmitTarget::HelpSearch, "", "prompt", 1);
        let mut help = Self {
            input,
            scope: HelpScope::Active,
            previous_mode,
            snapshot_mode: snapshot_mode.to_owned(),
            active,
            entries: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            detail_scroll: 0,
        };
        help.rebuild_entries();
        help
    }

    pub fn input_text(&self, ws: &Workspace) -> String {
        self.input.text(ws)
    }

    pub fn input_cursor_column(&self, ws: &mut Workspace) -> usize {
        self.input.cursor_column(ws)
    }

    /// Remove the underlying scratch editor. Called when the help modal is
    /// closed so the slot does not linger in the workspace.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
    }

    pub fn scope(&self) -> HelpScope {
        self.scope
    }

    pub fn snapshot_mode(&self) -> &str {
        &self.snapshot_mode
    }

    pub fn entries(&self) -> &[HelpEntry] {
        &self.entries
    }

    pub fn filtered(&self) -> &[usize] {
        &self.filtered
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_entry(&self) -> Option<&HelpEntry> {
        let idx = *self.filtered.get(self.selected)?;
        self.entries.get(idx)
    }

    pub fn detail_scroll(&self) -> u16 {
        self.detail_scroll
    }

    /// Refilter the help entries against the current input text. Called from
    /// `handle_insert_key` after the global prompt-mode short-circuit mutates
    /// the help input's buffer, so the list stays in sync without a dedicated
    /// post-edit hook.
    pub(crate) fn sync_filter(&mut self, ws: &Workspace) {
        self.refilter(ws);
    }

    pub(crate) fn dispatch_selected_pub(&self) -> HelpOutcome {
        self.dispatch_selected_inner()
    }

    pub(crate) fn move_selection(&mut self, delta: i32) {
        self.move_selection_inner(delta);
    }

    pub(crate) fn jump_selection(&mut self, target: usize) {
        self.jump_selection_inner(target);
    }

    pub(crate) fn scroll_detail(&mut self, delta: i32) {
        self.scroll_detail_inner(delta);
    }

    pub(crate) fn toggle_scope_pub(&mut self, ws: &Workspace) {
        self.toggle_scope(ws);
    }

    fn dispatch_selected_inner(&self) -> HelpOutcome {
        let Some(entry) = self.selected_entry() else {
            return HelpOutcome::None;
        };
        let registry_entry = match registry::lookup(entry.def.name()) {
            Some(e) => e,
            None => return HelpOutcome::None,
        };
        let params = entry.def.params();
        if params.is_empty() {
            return HelpOutcome::Dispatch(registry_entry, Vec::new());
        }
        if entry.bound_args.len() < params.len() {
            return HelpOutcome::None;
        }
        let mut values = Vec::with_capacity(params.len());
        for arg in &entry.bound_args {
            match arg_to_param_value(arg) {
                Some(v) => values.push(v),
                None => return HelpOutcome::None,
            }
        }
        HelpOutcome::Dispatch(registry_entry, values)
    }

    fn move_selection_inner(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let max = (self.filtered.len() - 1) as i32;
        self.selected = (self.selected as i32 + delta).clamp(0, max) as usize;
        self.detail_scroll = 0;
    }

    fn jump_selection_inner(&mut self, target: usize) {
        self.selected = target.min(self.filtered.len().saturating_sub(1));
        self.detail_scroll = 0;
    }

    fn scroll_detail_inner(&mut self, delta: i32) {
        if delta < 0 {
            self.detail_scroll = self.detail_scroll.saturating_sub((-delta) as u16);
        } else {
            self.detail_scroll = self.detail_scroll.saturating_add(delta as u16);
        }
    }

    fn toggle_scope(&mut self, ws: &Workspace) {
        self.scope = match self.scope {
            HelpScope::Active => HelpScope::All,
            HelpScope::All => HelpScope::Active,
        };
        self.entries = match self.scope {
            HelpScope::Active => build_active_entries(&self.active),
            HelpScope::All => build_all_entries(),
        };
        self.refilter(ws);
    }

    /// Rebuild entries from the current scope. Called at construction, before
    /// a workspace is available for reading the input rope, so the refilter
    /// step assumes an empty needle.
    fn rebuild_entries(&mut self) {
        self.entries = match self.scope {
            HelpScope::Active => build_active_entries(&self.active),
            HelpScope::All => build_all_entries(),
        };
        self.filtered = (0..self.entries.len()).collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
        self.detail_scroll = 0;
    }

    fn refilter(&mut self, ws: &Workspace) {
        let needle = self.input.text(ws).to_lowercase();

        if needle.is_empty() {
            // Preserve entries order (sorted by key label for Active, by name
            // for All) so users see a stable reference shape before typing.
            self.filtered = (0..self.entries.len()).collect();
            if self.selected >= self.filtered.len() {
                self.selected = self.filtered.len().saturating_sub(1);
            }
            self.detail_scroll = 0;
            return;
        }

        let mut prefix_name: Vec<usize> = Vec::new();
        let mut substring_name: Vec<usize> = Vec::new();
        let mut key_match: Vec<usize> = Vec::new();
        let mut short_match: Vec<usize> = Vec::new();
        let mut long_match: Vec<usize> = Vec::new();

        for (i, entry) in self.entries.iter().enumerate() {
            let name_lc = entry.def.name().to_lowercase();
            if name_lc.starts_with(&needle) {
                prefix_name.push(i);
                continue;
            }
            if name_lc.contains(&needle) {
                substring_name.push(i);
                continue;
            }
            if let Some(label) = entry.key_label.as_deref() {
                if label.to_lowercase().contains(&needle) {
                    key_match.push(i);
                    continue;
                }
            }
            if entry.def.short_desc().to_lowercase().contains(&needle) {
                short_match.push(i);
                continue;
            }
            if entry.def.long_desc().to_lowercase().contains(&needle) {
                long_match.push(i);
            }
        }

        let sort = |v: &mut Vec<usize>, entries: &[HelpEntry]| {
            v.sort_by(|&a, &b| entries[a].def.name().cmp(entries[b].def.name()));
        };
        sort(&mut prefix_name, &self.entries);
        sort(&mut substring_name, &self.entries);
        sort(&mut key_match, &self.entries);
        sort(&mut short_match, &self.entries);
        sort(&mut long_match, &self.entries);

        let mut filtered = prefix_name;
        filtered.extend(substring_name);
        filtered.extend(key_match);
        filtered.extend(short_match);
        filtered.extend(long_match);

        self.filtered = filtered;
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
        self.detail_scroll = 0;
    }
}

fn build_active_entries(active: &[(String, Vec<ResolvedAction>)]) -> Vec<HelpEntry> {
    let mut entries = Vec::new();
    for (label, actions) in active {
        let Some(first) = actions.first() else {
            continue;
        };
        let def: &'static dyn ActionDef = if first.name == "SetMode" {
            &SET_MODE_DEF
        } else {
            match registry::lookup(&first.name) {
                Some(reg) => reg.def,
                None => continue,
            }
        };
        entries.push(HelpEntry {
            def,
            key_label: Some(label.clone()),
            bound_args: first.args.clone(),
        });
    }
    entries.sort_by(|a, b| {
        a.key_label
            .as_deref()
            .unwrap_or("")
            .cmp(b.key_label.as_deref().unwrap_or(""))
            .then_with(|| a.def.name().cmp(b.def.name()))
    });
    entries
}

fn build_all_entries() -> Vec<HelpEntry> {
    let mut entries: Vec<HelpEntry> = registry::all()
        .filter(|e| e.def.palette_visible())
        .map(|e| HelpEntry {
            def: e.def,
            key_label: None,
            bound_args: Vec::new(),
        })
        .collect();
    entries.sort_by(|a, b| a.def.name().cmp(b.def.name()));
    entries
}

fn arg_to_param_value(arg: &ResolvedArg) -> Option<ParamValue> {
    match &arg.value {
        Value::String(s) => Some(ParamValue::String(s.clone())),
        Value::Ident(s) => Some(ParamValue::String(s.clone())),
        Value::Number(n) => Some(ParamValue::Number(*n)),
        Value::Bool(b) => Some(ParamValue::Bool(*b)),
        _ => None,
    }
}

pub fn format_arg(arg: &ResolvedArg) -> Option<String> {
    match &arg.value {
        Value::String(s) => Some(format!("\"{s}\"")),
        Value::Ident(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_harness::{keys, TestHarness};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn active_binding(label: &str, action_name: &str) -> (String, Vec<ResolvedAction>) {
        (
            label.to_owned(),
            vec![ResolvedAction {
                name: action_name.to_owned(),
                args: Vec::new(),
            }],
        )
    }

    fn active_binding_with_arg(
        label: &str,
        action_name: &str,
        arg_value: Value,
    ) -> (String, Vec<ResolvedAction>) {
        (
            label.to_owned(),
            vec![ResolvedAction {
                name: action_name.to_owned(),
                args: vec![ResolvedArg {
                    name: None,
                    value: arg_value,
                }],
            }],
        )
    }

    fn sample_active() -> Vec<(String, Vec<ResolvedAction>)> {
        vec![
            active_binding("q", "Quit"),
            active_binding("h", "MoveLeft"),
            active_binding("j", "MoveDown"),
            active_binding("k", "MoveUp"),
            active_binding("l", "MoveRight"),
        ]
    }

    /// Open a help modal in the given test harness with the supplied synthetic
    /// `active` bindings. Also transitions `stoat.mode` to `"prompt"` so help
    /// starts in its Insert sub-mode, matching the real `OpenHelp` handler.
    fn open_help_with(h: &mut TestHarness, active: Vec<(String, Vec<ResolvedAction>)>) {
        let previous_mode = h.stoat.mode.clone();
        let executor = h.stoat.executor.clone();
        let active_idx = h.stoat.active_workspace;
        let ws = &mut h.stoat.workspaces[active_idx];
        h.stoat.help = Some(Help::new("normal", active, ws, executor, previous_mode));
        h.stoat.mode = "prompt".into();
    }

    /// Dispatch a key through the test harness's top-level key handler so
    /// typing flows through `dispatch_help_key` with workspace access.
    fn send_key(h: &mut TestHarness, key: KeyEvent) {
        use crossterm::event::Event;
        h.stoat.update(Event::Key(key));
    }

    fn type_str(h: &mut TestHarness, s: &str) {
        for ch in s.chars() {
            send_key(h, keys::key(KeyCode::Char(ch)));
        }
    }

    fn help_ref(h: &TestHarness) -> &Help {
        h.stoat.help.as_ref().expect("help open")
    }

    fn filtered_names(h: &TestHarness) -> Vec<&'static str> {
        let help = help_ref(h);
        help.filtered()
            .iter()
            .map(|&i| help.entries()[i].def.name())
            .collect()
    }

    fn selected_name(h: &TestHarness) -> Option<&'static str> {
        help_ref(h).selected_entry().map(|e| e.def.name())
    }

    fn input_text(h: &mut TestHarness) -> String {
        let active_idx = h.stoat.active_workspace;
        let ws = &h.stoat.workspaces[active_idx];
        help_ref(h).input_text(ws)
    }

    #[test]
    fn new_active_scope_snapshots_mode_and_bindings() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        {
            let help = help_ref(&h);
            assert_eq!(help.snapshot_mode(), "normal");
            assert_eq!(help.scope(), HelpScope::Active);
        }
        assert_eq!(help_input_mode(&h.stoat.mode), HelpInput::Insert);
        let names = filtered_names(&h);
        assert!(names.contains(&"Quit"));
        assert!(names.contains(&"MoveDown"));
        assert_eq!(names.len(), 5);
    }

    #[test]
    fn new_all_scope_after_toggle_lists_palette_visible_actions() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        send_key(&mut h, keys::key(KeyCode::BackTab));
        assert_eq!(help_ref(&h).scope(), HelpScope::All);
        let names = filtered_names(&h);
        assert!(names.contains(&"Quit"));
        assert!(names.contains(&"OpenFile"));
        assert!(!names.contains(&"OpenCommandPalette"));
        assert!(!names.contains(&"OpenHelp"));
    }

    #[test]
    fn typing_filters_by_name_prefix() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        send_key(&mut h, keys::key(KeyCode::BackTab));
        type_str(&mut h, "Foc");
        let names = filtered_names(&h);
        assert!(!names.is_empty());
        assert!(names.contains(&"FocusLeft"));
        assert!(
            names[0].starts_with("Focus"),
            "expected name-prefix hit first, got {names:?}"
        );
    }

    #[test]
    fn typing_filters_by_short_desc_substring() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        send_key(&mut h, keys::key(KeyCode::BackTab));
        type_str(&mut h, "exit stoat");
        assert_eq!(filtered_names(&h), vec!["QuitAll"]);
    }

    #[test]
    fn typing_filters_by_key_label() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        type_str(&mut h, "j");
        assert!(filtered_names(&h).contains(&"MoveDown"));
    }

    #[test]
    fn shift_tab_toggles_scope() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        assert_eq!(help_ref(&h).scope(), HelpScope::Active);
        send_key(&mut h, keys::key(KeyCode::BackTab));
        assert_eq!(help_ref(&h).scope(), HelpScope::All);
        send_key(&mut h, keys::key(KeyCode::BackTab));
        assert_eq!(help_ref(&h).scope(), HelpScope::Active);
    }

    #[test]
    fn down_up_move_selection_and_reset_scroll() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        h.stoat.help.as_mut().unwrap().detail_scroll = 7;
        send_key(&mut h, keys::key(KeyCode::Down));
        assert_eq!(help_ref(&h).selected(), 1);
        assert_eq!(help_ref(&h).detail_scroll(), 0);
        h.stoat.help.as_mut().unwrap().detail_scroll = 3;
        send_key(&mut h, keys::key(KeyCode::Up));
        assert_eq!(help_ref(&h).selected(), 0);
        assert_eq!(help_ref(&h).detail_scroll(), 0);
    }

    #[test]
    fn ctrl_d_scrolls_detail_forward_ctrl_u_scrolls_back() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        send_key(&mut h, keys::ctrl('d'));
        assert_eq!(help_ref(&h).detail_scroll(), 5);
        send_key(&mut h, keys::ctrl('d'));
        assert_eq!(help_ref(&h).detail_scroll(), 10);
        send_key(&mut h, keys::ctrl('u'));
        assert_eq!(help_ref(&h).detail_scroll(), 5);
    }

    #[test]
    fn esc_in_insert_switches_to_normal() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        assert_eq!(help_input_mode(&h.stoat.mode), HelpInput::Insert);
        send_key(&mut h, keys::key(KeyCode::Esc));
        assert_eq!(help_input_mode(&h.stoat.mode), HelpInput::Normal);
    }

    #[test]
    fn esc_in_normal_closes_help() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        send_key(&mut h, keys::key(KeyCode::Esc));
        send_key(&mut h, keys::key(KeyCode::Esc));
        assert!(h.stoat.help.is_none());
    }

    #[test]
    fn i_returns_to_insert_from_normal() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        send_key(&mut h, keys::key(KeyCode::Esc));
        send_key(&mut h, keys::key(KeyCode::Char('i')));
        assert_eq!(help_input_mode(&h.stoat.mode), HelpInput::Insert);
    }

    #[test]
    fn normal_mode_j_k_navigate() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        send_key(&mut h, keys::key(KeyCode::Esc));
        send_key(&mut h, keys::key(KeyCode::Char('j')));
        assert_eq!(help_ref(&h).selected(), 1);
        send_key(&mut h, keys::key(KeyCode::Char('k')));
        assert_eq!(help_ref(&h).selected(), 0);
    }

    #[test]
    fn normal_mode_g_jumps_to_top() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        send_key(&mut h, keys::key(KeyCode::Down));
        send_key(&mut h, keys::key(KeyCode::Down));
        send_key(&mut h, keys::key(KeyCode::Esc));
        send_key(&mut h, keys::key(KeyCode::Char('g')));
        assert_eq!(help_ref(&h).selected(), 0);
    }

    #[test]
    fn normal_mode_shift_g_jumps_to_bottom() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        send_key(&mut h, keys::key(KeyCode::Esc));
        send_key(
            &mut h,
            KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
        );
        let last = help_ref(&h).filtered().len() - 1;
        assert_eq!(help_ref(&h).selected(), last);
    }

    #[test]
    fn enter_zero_arg_closes_help_on_dispatch() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        type_str(&mut h, "Quit");
        assert_eq!(selected_name(&h), Some("Quit"));
        send_key(&mut h, keys::key(KeyCode::Enter));
        assert!(h.stoat.help.is_none(), "Dispatch should close help");
    }

    #[test]
    fn enter_bound_entry_with_args_dispatches() {
        let active = vec![active_binding_with_arg(
            "C-o",
            "OpenFile",
            Value::String("/tmp/x.rs".to_owned()),
        )];
        let mut h = TestHarness::default();
        open_help_with(&mut h, active);
        send_key(&mut h, keys::key(KeyCode::Enter));
        assert!(h.stoat.help.is_none(), "Dispatch should close help");
    }

    #[test]
    fn enter_unbound_param_action_is_noop() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, Vec::new());
        send_key(&mut h, keys::key(KeyCode::BackTab));
        type_str(&mut h, "OpenFile");
        assert_eq!(selected_name(&h), Some("OpenFile"));
        send_key(&mut h, keys::key(KeyCode::Enter));
        assert!(
            h.stoat.help.is_some(),
            "unbound param action should stay open"
        );
    }

    #[test]
    fn selection_clamps_after_narrowing_filter() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        send_key(&mut h, keys::key(KeyCode::Down));
        send_key(&mut h, keys::key(KeyCode::Down));
        type_str(&mut h, "Quit");
        assert_eq!(help_ref(&h).selected(), 0);
    }

    #[test]
    fn utf8_query_safe() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        send_key(&mut h, keys::key(KeyCode::Char('é')));
        assert_eq!(input_text(&mut h), "é");
        send_key(&mut h, keys::key(KeyCode::Backspace));
        assert_eq!(input_text(&mut h), "");
    }

    #[test]
    fn backspace_refilters() {
        let mut h = TestHarness::default();
        open_help_with(&mut h, sample_active());
        send_key(&mut h, keys::key(KeyCode::BackTab));
        type_str(&mut h, "Focus");
        let narrow = help_ref(&h).filtered().len();
        for _ in 0..5 {
            send_key(&mut h, keys::key(KeyCode::Backspace));
        }
        assert_eq!(input_text(&mut h), "");
        assert!(help_ref(&h).filtered().len() > narrow);
    }

    #[test]
    fn help_opens_searches_closes_end_to_end() {
        let mut h = crate::Stoat::test();
        h.type_keys("?");
        h.type_text("quit");
        h.type_keys("escape escape");
        let frame = h.snapshot();
        assert_eq!(frame.mode, "normal");
    }

    #[test]
    fn active_scope_surfaces_set_mode_bindings() {
        let active = vec![active_binding_with_arg(
            "i",
            "SetMode",
            Value::Ident("insert".to_owned()),
        )];
        let mut h = TestHarness::default();
        open_help_with(&mut h, active);
        assert_eq!(filtered_names(&h), vec!["SetMode"]);
        let help = help_ref(&h);
        let entry = help.selected_entry().unwrap();
        assert_eq!(entry.def.name(), "SetMode");
        assert_eq!(entry.key_label.as_deref(), Some("i"));
        assert!(!entry.bound_args.is_empty());
    }

    #[test]
    fn snapshot_help_active_default() {
        let mut h = crate::Stoat::test();
        h.type_keys("?");
        h.assert_snapshot("help_active_default");
    }

    #[test]
    fn snapshot_help_filter_typing() {
        let mut h = crate::Stoat::test();
        h.type_keys("?");
        h.type_text("move");
        h.assert_snapshot("help_filter_typing");
    }

    #[test]
    fn snapshot_help_all_scope_after_shift_tab() {
        let mut h = crate::Stoat::test();
        h.type_keys("?");
        h.type_keys("backtab");
        h.assert_snapshot("help_all_scope_after_shift_tab");
    }

    #[test]
    fn snapshot_help_normal_mode() {
        let mut h = crate::Stoat::test();
        h.type_keys("?");
        h.type_keys("escape");
        h.assert_snapshot("help_normal_mode");
    }
}
