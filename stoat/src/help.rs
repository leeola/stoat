use crate::{
    keymap::{ResolvedAction, ResolvedArg},
    run::CommandBuffer,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use stoat_action::{registry, ActionDef, ActionKind, ParamDef, ParamValue};
use stoat_config::Value;

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
    input: CommandBuffer,
    input_mode: HelpInput,
    scope: HelpScope,
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
    pub fn new(snapshot_mode: &str, active: Vec<(String, Vec<ResolvedAction>)>) -> Self {
        let mut help = Self {
            input: CommandBuffer::new(),
            input_mode: HelpInput::Insert,
            scope: HelpScope::Active,
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

    pub fn input(&self) -> &str {
        self.input.as_str()
    }

    pub fn input_cursor_column(&self) -> usize {
        self.input.cursor_column()
    }

    pub fn input_mode(&self) -> HelpInput {
        self.input_mode
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

    pub fn handle_key(&mut self, key: KeyEvent) -> HelpOutcome {
        match self.input_mode {
            HelpInput::Insert => self.handle_insert_key(key),
            HelpInput::Normal => self.handle_normal_key(key),
        }
    }

    fn handle_insert_key(&mut self, key: KeyEvent) -> HelpOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            KeyCode::Esc => {
                self.input_mode = HelpInput::Normal;
                HelpOutcome::None
            },
            KeyCode::Enter => self.dispatch_selected(),
            KeyCode::Up => {
                self.move_selection(-1);
                HelpOutcome::None
            },
            KeyCode::Down => {
                self.move_selection(1);
                HelpOutcome::None
            },
            KeyCode::Char('p') if ctrl => {
                self.move_selection(-1);
                HelpOutcome::None
            },
            KeyCode::Char('n') if ctrl => {
                self.move_selection(1);
                HelpOutcome::None
            },
            KeyCode::Char('u') if ctrl => {
                self.scroll_detail(-5);
                HelpOutcome::None
            },
            KeyCode::Char('d') if ctrl => {
                self.scroll_detail(5);
                HelpOutcome::None
            },
            KeyCode::PageUp => {
                self.scroll_detail(-5);
                HelpOutcome::None
            },
            KeyCode::PageDown => {
                self.scroll_detail(5);
                HelpOutcome::None
            },
            KeyCode::BackTab => {
                self.toggle_scope();
                HelpOutcome::None
            },
            KeyCode::Backspace => {
                self.input.delete_backward();
                self.refilter();
                HelpOutcome::None
            },
            KeyCode::Left => {
                self.input.move_left();
                HelpOutcome::None
            },
            KeyCode::Right => {
                self.input.move_right();
                HelpOutcome::None
            },
            KeyCode::Home => {
                self.input.move_home();
                HelpOutcome::None
            },
            KeyCode::End => {
                self.input.move_end();
                HelpOutcome::None
            },
            KeyCode::Char(c) if !ctrl && !alt => {
                self.input.insert_char(c);
                self.refilter();
                HelpOutcome::None
            },
            _ => HelpOutcome::None,
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> HelpOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Esc => HelpOutcome::Close,
            KeyCode::Char('i') if !ctrl => {
                self.input_mode = HelpInput::Insert;
                HelpOutcome::None
            },
            KeyCode::Enter => self.dispatch_selected(),
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1);
                HelpOutcome::None
            },
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1);
                HelpOutcome::None
            },
            KeyCode::Char('g') if !ctrl => {
                self.jump_selection(0);
                HelpOutcome::None
            },
            KeyCode::Char('G') if !ctrl => {
                let last = self.filtered.len().saturating_sub(1);
                self.jump_selection(last);
                HelpOutcome::None
            },
            KeyCode::Char('u') if ctrl => {
                self.scroll_detail(-5);
                HelpOutcome::None
            },
            KeyCode::Char('d') if ctrl => {
                self.scroll_detail(5);
                HelpOutcome::None
            },
            KeyCode::PageUp => {
                self.scroll_detail(-5);
                HelpOutcome::None
            },
            KeyCode::PageDown => {
                self.scroll_detail(5);
                HelpOutcome::None
            },
            KeyCode::BackTab => {
                self.toggle_scope();
                HelpOutcome::None
            },
            _ => HelpOutcome::None,
        }
    }

    fn dispatch_selected(&mut self) -> HelpOutcome {
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

    fn move_selection(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let max = (self.filtered.len() - 1) as i32;
        self.selected = (self.selected as i32 + delta).clamp(0, max) as usize;
        self.detail_scroll = 0;
    }

    fn jump_selection(&mut self, target: usize) {
        self.selected = target.min(self.filtered.len().saturating_sub(1));
        self.detail_scroll = 0;
    }

    fn scroll_detail(&mut self, delta: i32) {
        if delta < 0 {
            self.detail_scroll = self.detail_scroll.saturating_sub((-delta) as u16);
        } else {
            self.detail_scroll = self.detail_scroll.saturating_add(delta as u16);
        }
    }

    fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            HelpScope::Active => HelpScope::All,
            HelpScope::All => HelpScope::Active,
        };
        self.rebuild_entries();
    }

    fn rebuild_entries(&mut self) {
        self.entries = match self.scope {
            HelpScope::Active => build_active_entries(&self.active),
            HelpScope::All => build_all_entries(),
        };
        self.refilter();
    }

    fn refilter(&mut self) {
        let needle = self.input.as_str().to_lowercase();

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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

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

    fn type_str(help: &mut Help, s: &str) {
        for ch in s.chars() {
            help.handle_key(key(KeyCode::Char(ch)));
        }
    }

    fn selected_name(help: &Help) -> Option<&'static str> {
        help.selected_entry().map(|e| e.def.name())
    }

    fn filtered_names(help: &Help) -> Vec<&'static str> {
        help.filtered()
            .iter()
            .map(|&i| help.entries()[i].def.name())
            .collect()
    }

    #[test]
    fn new_active_scope_snapshots_mode_and_bindings() {
        let help = Help::new("normal", sample_active());
        assert_eq!(help.snapshot_mode(), "normal");
        assert_eq!(help.scope(), HelpScope::Active);
        assert_eq!(help.input_mode(), HelpInput::Insert);
        let names = filtered_names(&help);
        assert!(names.contains(&"Quit"));
        assert!(names.contains(&"MoveDown"));
        assert_eq!(names.len(), 5);
    }

    #[test]
    fn new_all_scope_after_toggle_lists_palette_visible_actions() {
        let mut help = Help::new("normal", sample_active());
        help.handle_key(key(KeyCode::BackTab));
        assert_eq!(help.scope(), HelpScope::All);
        let names = filtered_names(&help);
        assert!(names.contains(&"Quit"));
        assert!(names.contains(&"OpenFile"));
        assert!(!names.contains(&"OpenCommandPalette"));
        assert!(!names.contains(&"OpenHelp"));
    }

    #[test]
    fn typing_filters_by_name_prefix() {
        let mut help = Help::new("normal", sample_active());
        help.handle_key(key(KeyCode::BackTab));
        type_str(&mut help, "Foc");
        let names = filtered_names(&help);
        assert!(!names.is_empty());
        assert!(names.contains(&"FocusLeft"));
        // Name-prefix matches rank ahead of other buckets.
        assert!(
            names[0].starts_with("Focus"),
            "expected name-prefix hit first, got {names:?}"
        );
    }

    #[test]
    fn typing_filters_by_short_desc_substring() {
        let mut help = Help::new("normal", sample_active());
        help.handle_key(key(KeyCode::BackTab));
        type_str(&mut help, "exit stoat");
        let names = filtered_names(&help);
        assert_eq!(names, vec!["Quit"]);
    }

    #[test]
    fn typing_filters_by_key_label() {
        let mut help = Help::new("normal", sample_active());
        type_str(&mut help, "j");
        let names = filtered_names(&help);
        assert!(names.contains(&"MoveDown"));
    }

    #[test]
    fn shift_tab_toggles_scope() {
        let mut help = Help::new("normal", sample_active());
        assert_eq!(help.scope(), HelpScope::Active);
        help.handle_key(key(KeyCode::BackTab));
        assert_eq!(help.scope(), HelpScope::All);
        help.handle_key(key(KeyCode::BackTab));
        assert_eq!(help.scope(), HelpScope::Active);
    }

    #[test]
    fn down_up_move_selection_and_reset_scroll() {
        let mut help = Help::new("normal", sample_active());
        help.detail_scroll = 7;
        help.handle_key(key(KeyCode::Down));
        assert_eq!(help.selected(), 1);
        assert_eq!(help.detail_scroll(), 0);
        help.detail_scroll = 3;
        help.handle_key(key(KeyCode::Up));
        assert_eq!(help.selected(), 0);
        assert_eq!(help.detail_scroll(), 0);
    }

    #[test]
    fn ctrl_d_scrolls_detail_forward_ctrl_u_scrolls_back() {
        let mut help = Help::new("normal", sample_active());
        help.handle_key(ctrl('d'));
        assert_eq!(help.detail_scroll(), 5);
        help.handle_key(ctrl('d'));
        assert_eq!(help.detail_scroll(), 10);
        help.handle_key(ctrl('u'));
        assert_eq!(help.detail_scroll(), 5);
    }

    #[test]
    fn esc_in_insert_switches_to_normal() {
        let mut help = Help::new("normal", sample_active());
        assert_eq!(help.input_mode(), HelpInput::Insert);
        help.handle_key(key(KeyCode::Esc));
        assert_eq!(help.input_mode(), HelpInput::Normal);
    }

    #[test]
    fn esc_in_normal_returns_close() {
        let mut help = Help::new("normal", sample_active());
        help.handle_key(key(KeyCode::Esc));
        assert!(matches!(
            help.handle_key(key(KeyCode::Esc)),
            HelpOutcome::Close
        ));
    }

    #[test]
    fn i_returns_to_insert_from_normal() {
        let mut help = Help::new("normal", sample_active());
        help.handle_key(key(KeyCode::Esc));
        help.handle_key(key(KeyCode::Char('i')));
        assert_eq!(help.input_mode(), HelpInput::Insert);
    }

    #[test]
    fn normal_mode_j_k_navigate() {
        let mut help = Help::new("normal", sample_active());
        help.handle_key(key(KeyCode::Esc));
        help.handle_key(key(KeyCode::Char('j')));
        assert_eq!(help.selected(), 1);
        help.handle_key(key(KeyCode::Char('k')));
        assert_eq!(help.selected(), 0);
    }

    #[test]
    fn normal_mode_g_jumps_to_top() {
        let mut help = Help::new("normal", sample_active());
        help.handle_key(key(KeyCode::Down));
        help.handle_key(key(KeyCode::Down));
        help.handle_key(key(KeyCode::Esc));
        help.handle_key(key(KeyCode::Char('g')));
        assert_eq!(help.selected(), 0);
    }

    #[test]
    fn normal_mode_shift_g_jumps_to_bottom() {
        let mut help = Help::new("normal", sample_active());
        help.handle_key(key(KeyCode::Esc));
        help.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
        assert_eq!(help.selected(), help.filtered().len() - 1);
    }

    #[test]
    fn enter_zero_arg_returns_dispatch() {
        let mut help = Help::new("normal", sample_active());
        type_str(&mut help, "Quit");
        assert_eq!(selected_name(&help), Some("Quit"));
        let outcome = help.handle_key(key(KeyCode::Enter));
        match outcome {
            HelpOutcome::Dispatch(entry, params) => {
                assert_eq!(entry.def.name(), "Quit");
                assert!(params.is_empty());
            },
            _ => panic!("expected Dispatch"),
        }
    }

    #[test]
    fn enter_bound_entry_with_args_dispatches_with_params() {
        let active = vec![active_binding_with_arg(
            "C-o",
            "OpenFile",
            Value::String("/tmp/x.rs".to_owned()),
        )];
        let mut help = Help::new("normal", active);
        let outcome = help.handle_key(key(KeyCode::Enter));
        match outcome {
            HelpOutcome::Dispatch(entry, params) => {
                assert_eq!(entry.def.name(), "OpenFile");
                assert_eq!(params, vec![ParamValue::String("/tmp/x.rs".into())]);
            },
            _ => panic!("expected Dispatch"),
        }
    }

    #[test]
    fn enter_unbound_param_action_is_noop() {
        let mut help = Help::new("normal", Vec::new());
        help.handle_key(key(KeyCode::BackTab));
        type_str(&mut help, "OpenFile");
        assert_eq!(selected_name(&help), Some("OpenFile"));
        assert!(matches!(
            help.handle_key(key(KeyCode::Enter)),
            HelpOutcome::None
        ));
    }

    #[test]
    fn selection_clamps_after_narrowing_filter() {
        let mut help = Help::new("normal", sample_active());
        help.handle_key(key(KeyCode::Down));
        help.handle_key(key(KeyCode::Down));
        type_str(&mut help, "Quit");
        assert_eq!(help.selected(), 0);
    }

    #[test]
    fn utf8_query_safe() {
        let mut help = Help::new("normal", sample_active());
        help.handle_key(key(KeyCode::Char('é')));
        assert_eq!(help.input(), "é");
        help.handle_key(key(KeyCode::Backspace));
        assert_eq!(help.input(), "");
    }

    #[test]
    fn backspace_refilters() {
        let mut help = Help::new("normal", sample_active());
        help.handle_key(key(KeyCode::BackTab));
        type_str(&mut help, "Focus");
        let narrow = help.filtered().len();
        help.handle_key(key(KeyCode::Backspace));
        help.handle_key(key(KeyCode::Backspace));
        help.handle_key(key(KeyCode::Backspace));
        help.handle_key(key(KeyCode::Backspace));
        help.handle_key(key(KeyCode::Backspace));
        assert_eq!(help.input(), "");
        assert!(help.filtered().len() > narrow);
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
        let help = Help::new("normal", active);
        let names: Vec<_> = help
            .filtered()
            .iter()
            .map(|&i| help.entries()[i].def.name())
            .collect();
        assert_eq!(names, vec!["SetMode"]);
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
