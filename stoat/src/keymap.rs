use compact_str::CompactString;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use stoat_action::{registry, Action, ParamValue};
use stoat_config::{
    ActionExpr, Binding, Config, EventType, Key, KeyPart, Predicate, Statement, Value,
};

/// The mode the input state machine starts in. Bindings scoped to it,
/// or to no mode at all, need no prefix chord to reach.
const BASE_MODE: &str = "normal";

/// Recursion cap for [`Keymap::chord_for_action`] mode-prefix tracing,
/// so a pathological cyclic `SetMode` chain cannot loop forever.
const MAX_MODE_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq)]
pub enum StateValue {
    String(CompactString),
    Number(f64),
    Bool(bool),
}

pub trait KeymapState {
    fn get(&self, field: &str) -> Option<&StateValue>;
}

/// Whether `mode` is one where a printable key is typed literally as
/// text rather than dispatched as a binding. The single source for
/// both the text-input focus gate and the `input_active` predicate
/// flag exposed through [`KeymapState`].
pub fn is_text_input_mode(mode: &str) -> bool {
    matches!(mode, "insert" | "reword_insert" | "prompt" | "run")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledKey {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl CompiledKey {
    pub fn from_key_part(kp: &KeyPart) -> Option<Self> {
        if kp.keys.is_empty() {
            return None;
        }

        let mut modifiers = KeyModifiers::empty();
        let mut key_idx = 0;

        for (i, k) in kp.keys.iter().enumerate() {
            if i == kp.keys.len() - 1 {
                key_idx = i;
                break;
            }
            match k {
                Key::Char('C') => modifiers |= KeyModifiers::CONTROL,
                Key::Char('S') => modifiers |= KeyModifiers::SHIFT,
                Key::Char('A') => modifiers |= KeyModifiers::ALT,
                Key::Named(n) if n == "Ctrl" => modifiers |= KeyModifiers::CONTROL,
                Key::Named(n) if n == "Shift" => modifiers |= KeyModifiers::SHIFT,
                Key::Named(n) if n == "Alt" => modifiers |= KeyModifiers::ALT,
                Key::Named(n) if n == "Cmd" => modifiers |= KeyModifiers::SUPER,
                _ => {
                    key_idx = i;
                    break;
                },
            }
        }

        let code = resolve_key(&kp.keys[key_idx])?;
        Some(Self { code, modifiers })
    }

    pub fn matches(&self, event: &KeyEvent) -> bool {
        event.code == self.code && event.modifiers == self.modifiers
    }

    pub fn to_key_token(&self) -> String {
        let base = match self.code {
            KeyCode::Char(' ') => "space",
            KeyCode::Char(c) => return self.format_with_modifiers(&c.to_string()),
            KeyCode::Esc => "escape",
            KeyCode::Enter => "enter",
            KeyCode::Tab => "tab",
            KeyCode::Backspace => "backspace",
            KeyCode::Delete => "delete",
            KeyCode::Up => "up",
            KeyCode::Down => "down",
            KeyCode::Left => "left",
            KeyCode::Right => "right",
            KeyCode::Home => "home",
            KeyCode::End => "end",
            KeyCode::PageUp => "pageup",
            KeyCode::PageDown => "pagedown",
            KeyCode::F(n) => return self.format_with_modifiers(&format!("f{n}")),
            _ => "?",
        };
        self.format_with_modifiers(base)
    }

    fn format_with_modifiers(&self, base: &str) -> String {
        if self.modifiers.is_empty() {
            return base.to_string();
        }
        let mut parts = Vec::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("ctrl");
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            parts.push("shift");
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push("alt");
        }
        parts.push(base);
        parts.join("-")
    }

    pub fn display_label(&self) -> String {
        let mut parts = Vec::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("C".to_string());
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            parts.push("S".to_string());
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push("A".to_string());
        }
        parts.push(match self.code {
            KeyCode::Char(' ') => "Spc".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Esc => "Esc".to_string(),
            KeyCode::Enter => "Ret".to_string(),
            KeyCode::Tab => "Tab".to_string(),
            KeyCode::BackTab => "S-Tab".to_string(),
            KeyCode::Backspace => "Bsp".to_string(),
            KeyCode::Delete => "Del".to_string(),
            KeyCode::Up => "Up".to_string(),
            KeyCode::Down => "Dn".to_string(),
            KeyCode::Left => "Lt".to_string(),
            KeyCode::Right => "Rt".to_string(),
            KeyCode::Home => "Home".to_string(),
            KeyCode::End => "End".to_string(),
            KeyCode::PageUp => "PgUp".to_string(),
            KeyCode::PageDown => "PgDn".to_string(),
            KeyCode::Insert => "Ins".to_string(),
            KeyCode::F(n) => format!("F{n}"),
            _ => "?".to_string(),
        });
        parts.join("-")
    }
}

fn resolve_key(key: &Key) -> Option<KeyCode> {
    match key {
        Key::Char(c) => Some(KeyCode::Char(*c)),
        Key::Named(name) => match name.as_str() {
            "Space" => Some(KeyCode::Char(' ')),
            "Minus" => Some(KeyCode::Char('-')),
            "Escape" | "Esc" => Some(KeyCode::Esc),
            "Enter" | "Return" => Some(KeyCode::Enter),
            "Tab" => Some(KeyCode::Tab),
            "BackTab" => Some(KeyCode::BackTab),
            "Backspace" => Some(KeyCode::Backspace),
            "Delete" | "Del" => Some(KeyCode::Delete),
            "Up" => Some(KeyCode::Up),
            "Down" => Some(KeyCode::Down),
            "Left" => Some(KeyCode::Left),
            "Right" => Some(KeyCode::Right),
            "Home" => Some(KeyCode::Home),
            "End" => Some(KeyCode::End),
            "PageUp" => Some(KeyCode::PageUp),
            "PageDown" => Some(KeyCode::PageDown),
            "Insert" => Some(KeyCode::Insert),
            s if s.starts_with('F') => s[1..].parse::<u8>().ok().map(KeyCode::F),
            _ => None,
        },
    }
}

pub fn evaluate(predicate: &Predicate, state: &dyn KeymapState) -> bool {
    match predicate {
        Predicate::Eq(field, val) => match state.get(&field.node) {
            Some(sv) => state_value_eq(sv, &val.node),
            None => false,
        },
        Predicate::NotEq(field, val) => match state.get(&field.node) {
            Some(sv) => !state_value_eq(sv, &val.node),
            None => true,
        },
        Predicate::Gt(field, val) => state_value_cmp(state, &field.node, &val.node, |o| {
            o == std::cmp::Ordering::Greater
        }),
        Predicate::Lt(field, val) => state_value_cmp(state, &field.node, &val.node, |o| {
            o == std::cmp::Ordering::Less
        }),
        Predicate::Gte(field, val) => state_value_cmp(state, &field.node, &val.node, |o| {
            o != std::cmp::Ordering::Less
        }),
        Predicate::Lte(field, val) => state_value_cmp(state, &field.node, &val.node, |o| {
            o != std::cmp::Ordering::Greater
        }),
        Predicate::Matches(field, pattern) => match state.get(&field.node) {
            Some(StateValue::String(s)) => glob_match(&pattern.node, s),
            _ => false,
        },
        Predicate::Bool(field) => match state.get(&field.node) {
            Some(StateValue::Bool(b)) => *b,
            Some(_) => true,
            None => false,
        },
        Predicate::And(l, r) => evaluate(&l.node, state) && evaluate(&r.node, state),
        Predicate::Or(l, r) => evaluate(&l.node, state) || evaluate(&r.node, state),
    }
}

fn state_value_eq(sv: &StateValue, val: &Value) -> bool {
    match (sv, val) {
        (StateValue::String(s), Value::String(v)) => s == v,
        (StateValue::String(s), Value::Ident(v)) => s == v,
        (StateValue::Number(n), Value::Number(v)) => (n - v).abs() < f64::EPSILON,
        (StateValue::Bool(b), Value::Bool(v)) => b == v,
        _ => false,
    }
}

fn state_value_cmp(
    state: &dyn KeymapState,
    field: &str,
    val: &Value,
    check: impl Fn(std::cmp::Ordering) -> bool,
) -> bool {
    match (state.get(field), val) {
        (Some(StateValue::Number(n)), Value::Number(v)) => n.partial_cmp(v).is_some_and(check),
        _ => false,
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_inner(
        &pattern.chars().collect::<Vec<_>>(),
        &text.chars().collect::<Vec<_>>(),
        0,
        0,
    )
}

fn glob_match_inner(pattern: &[char], text: &[char], mut pi: usize, mut ti: usize) -> bool {
    while pi < pattern.len() {
        match pattern[pi] {
            '*' => {
                pi += 1;
                for skip in 0..=(text.len() - ti) {
                    if glob_match_inner(pattern, text, pi, ti + skip) {
                        return true;
                    }
                }
                return false;
            },
            '?' => {
                if ti >= text.len() {
                    return false;
                }
                pi += 1;
                ti += 1;
            },
            c => {
                if ti >= text.len() || text[ti] != c {
                    return false;
                }
                pi += 1;
                ti += 1;
            },
        }
    }
    ti >= text.len()
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAction {
    pub name: String,
    pub args: Vec<ResolvedArg>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedArg {
    pub name: Option<String>,
    pub value: Value,
}

/// Strip the `SHIFT` modifier from events where it duplicates information
/// already carried by the keycode, so bindings written without an explicit
/// `S-` prefix still match what the terminal emits.
///
/// Default crossterm without the kitty keyboard protocol reports Shift+a as
/// `(Char('A'), SHIFT)` and Shift-Tab (CSI Z) as `(BackTab, SHIFT)`, but
/// bindings written as `A` or `BackTab` compile to `(_, NONE)` and modifier
/// comparison in [`CompiledKey::matches`] is strict. For `Char(letter)` the
/// uppercase code already encodes Shift; for `BackTab` the keycode itself is
/// the Shift-Tab variant. In both cases the SHIFT modifier is redundant, so
/// dropping it up-front keeps bindings terminal-agnostic.
pub fn normalize_shift_event(key: KeyEvent) -> KeyEvent {
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

pub fn arg_as_str(arg: &ResolvedArg) -> Option<String> {
    match &arg.value {
        Value::String(s) => Some(s.clone()),
        Value::Ident(s) => Some(s.clone()),
        _ => None,
    }
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

pub fn resolve_action(name: &str, args: &[ResolvedArg]) -> Option<Box<dyn Action>> {
    let entry = registry::lookup(name)?;
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

#[derive(Debug, Clone)]
struct CompiledBinding {
    key: CompiledKey,
    predicates: Vec<Predicate>,
    actions: Vec<ResolvedAction>,
}

pub struct Keymap {
    bindings: Vec<CompiledBinding>,
}

impl Keymap {
    pub fn compile(config: &Config) -> Self {
        let mut functions: HashMap<String, Vec<stoat_config::Spanned<Statement>>> = HashMap::new();
        let mut bindings = Vec::new();

        for block in &config.blocks {
            if block.node.event != EventType::Key {
                continue;
            }
            collect_functions(&block.node.statements, &mut functions);
            compile_statements(&block.node.statements, &[], &functions, &mut bindings);
        }

        Self { bindings }
    }

    pub fn lookup(&self, state: &dyn KeymapState, event: &KeyEvent) -> Option<&[ResolvedAction]> {
        for binding in &self.bindings {
            if !binding.key.matches(event) {
                continue;
            }
            let preds_match = binding.predicates.iter().all(|p| evaluate(p, state));
            if preds_match {
                return Some(&binding.actions);
            }
        }
        None
    }

    pub fn active_keys(&self, state: &dyn KeymapState) -> Vec<(&CompiledKey, &[ResolvedAction])> {
        let mut results = Vec::new();
        for binding in &self.bindings {
            let matches = binding.predicates.iter().all(|p| evaluate(p, state));
            if matches {
                results.push((&binding.key, binding.actions.as_slice()));
            }
        }
        results
    }

    /// Returns `(key_label, actions)` for all bindings whose predicates match
    /// the current state. Uses the same evaluator as [`Keymap::lookup`].
    pub fn active_bindings(&self, state: &dyn KeymapState) -> Vec<(String, &[ResolvedAction])> {
        let mut results = Vec::new();
        for binding in &self.bindings {
            let matches = binding.predicates.iter().all(|p| evaluate(p, state));
            if matches {
                results.push((binding.key.display_label(), binding.actions.as_slice()));
            }
        }
        results
    }

    /// Returns active bindings that explicitly scope themselves by `scope_field`:
    /// at least one of the binding's predicates must reference that field.
    /// Used to surface only modal-specific keybinds (e.g. `help_open`) in hint
    /// popups, filtering out the broader mode-level bindings that also happen
    /// to match when the modal is open.
    pub fn scoped_bindings(
        &self,
        state: &dyn KeymapState,
        scope_field: &str,
    ) -> Vec<(String, &[ResolvedAction])> {
        let mut results = Vec::new();
        for binding in &self.bindings {
            let matches = binding.predicates.iter().all(|p| evaluate(p, state));
            if !matches {
                continue;
            }
            let in_scope = binding
                .predicates
                .iter()
                .any(|p| predicate_mentions_field(p, scope_field));
            if in_scope {
                results.push((binding.key.display_label(), binding.actions.as_slice()));
            }
        }
        results
    }

    /// The first chord that invokes `action_name`, or `None` when none does.
    ///
    /// The first element of [`Self::chords_for_action`]: the earliest
    /// binding in config order whose mode is reachable, so a direct
    /// normal-mode binding takes precedence over a deeper submode one.
    pub fn chord_for_action(&self, action_name: &str) -> Option<String> {
        self.chords_for_action(action_name).into_iter().next()
    }

    /// Every chord that invokes `action_name`, as display labels
    /// (e.g. `["Spc p"]`), in config order.
    ///
    /// Submode prefixes are reconstructed by tracing `SetMode` bindings back
    /// to the base `normal` mode; a binding whose mode has no reachable
    /// `SetMode` chain is skipped, and identical labels are deduped. Empty
    /// when no binding maps to the action.
    pub fn chords_for_action(&self, action_name: &str) -> Vec<String> {
        let mut labels = Vec::new();
        for binding in &self.bindings {
            if !binding.actions.iter().any(|a| a.name == action_name) {
                continue;
            }
            let Some(prefix) = self.mode_entry_prefix(binding_mode(binding), 0) else {
                continue;
            };
            let label = join_chord(prefix, &binding.key.display_label());
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        labels
    }

    /// The chord that switches into `mode` from the base mode; empty
    /// for the base `normal` mode or a binding with no mode predicate.
    /// `None` when no `SetMode` binding reaches `mode`. `depth` guards
    /// against cyclic `SetMode` chains.
    fn mode_entry_prefix(&self, mode: Option<&str>, depth: usize) -> Option<String> {
        let mode = match mode {
            None | Some(BASE_MODE) => return Some(String::new()),
            Some(m) => m,
        };
        if depth >= MAX_MODE_DEPTH {
            return None;
        }
        let entry = self.bindings.iter().find(|b| {
            b.actions
                .iter()
                .any(|a| a.name == "SetMode" && action_target_mode(a) == Some(mode))
        })?;
        let prefix = self.mode_entry_prefix(binding_mode(entry), depth + 1)?;
        Some(join_chord(prefix, &entry.key.display_label()))
    }
}

/// Join a mode-entry `prefix` and a `key` label into a space-separated
/// chord, dropping the separator when there is no prefix.
fn join_chord(prefix: String, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix} {key}")
    }
}

/// The mode a binding requires, read from its first `mode == X`
/// predicate; `None` when the binding is not mode-scoped.
fn binding_mode(binding: &CompiledBinding) -> Option<&str> {
    binding.predicates.iter().find_map(predicate_mode)
}

fn predicate_mode(pred: &Predicate) -> Option<&str> {
    match pred {
        Predicate::Eq(field, val) if field.node == "mode" => value_as_str(&val.node),
        Predicate::And(l, r) => predicate_mode(&l.node).or_else(|| predicate_mode(&r.node)),
        _ => None,
    }
}

/// The mode a `SetMode(mode)` action targets, from its first argument.
fn action_target_mode(action: &ResolvedAction) -> Option<&str> {
    action.args.first().and_then(|a| value_as_str(&a.value))
}

fn value_as_str(val: &Value) -> Option<&str> {
    match val {
        Value::String(s) | Value::Ident(s) => Some(s),
        _ => None,
    }
}

fn predicate_mentions_field(pred: &Predicate, field: &str) -> bool {
    match pred {
        Predicate::Eq(f, _)
        | Predicate::NotEq(f, _)
        | Predicate::Gt(f, _)
        | Predicate::Lt(f, _)
        | Predicate::Gte(f, _)
        | Predicate::Lte(f, _)
        | Predicate::Matches(f, _)
        | Predicate::Bool(f) => f.node == field,
        Predicate::And(l, r) | Predicate::Or(l, r) => {
            predicate_mentions_field(&l.node, field) || predicate_mentions_field(&r.node, field)
        },
    }
}

fn collect_functions(
    stmts: &[stoat_config::Spanned<Statement>],
    functions: &mut HashMap<String, Vec<stoat_config::Spanned<Statement>>>,
) {
    for stmt in stmts {
        if let Statement::FnDecl(decl) = &stmt.node {
            functions.insert(decl.name.node.clone(), decl.body.clone());
        }
    }
}

fn compile_statements(
    stmts: &[stoat_config::Spanned<Statement>],
    parent_predicates: &[Predicate],
    functions: &HashMap<String, Vec<stoat_config::Spanned<Statement>>>,
    out: &mut Vec<CompiledBinding>,
) {
    for stmt in stmts {
        match &stmt.node {
            Statement::Binding(binding) => {
                compile_binding(binding, parent_predicates, out);
            },
            Statement::PredicateBlock(block) => {
                let mut preds = parent_predicates.to_vec();
                preds.push(block.predicate.node.clone());
                compile_statements(&block.body, &preds, functions, out);
            },
            Statement::FnCall(name) => {
                if let Some(body) = functions.get(&name.node) {
                    compile_statements(body, parent_predicates, functions, out);
                }
            },
            Statement::FnDecl(_) | Statement::Setting(_) | Statement::Let(_) => {},
        }
    }
}

fn compile_binding(binding: &Binding, predicates: &[Predicate], out: &mut Vec<CompiledBinding>) {
    let Some(key) = CompiledKey::from_key_part(&binding.key.node) else {
        return;
    };

    let actions = match &binding.action.node {
        ActionExpr::Single(action) => vec![resolve_config_action(action)],
        ActionExpr::Sequence(actions) => actions
            .iter()
            .map(|a| resolve_config_action(&a.node))
            .collect(),
    };

    out.push(CompiledBinding {
        key,
        predicates: predicates.to_vec(),
        actions,
    });
}

pub(crate) fn resolve_config_action(action: &stoat_config::Action) -> ResolvedAction {
    let args = action
        .args
        .iter()
        .map(|a| match &a.node {
            stoat_config::Arg::Positional(val) => ResolvedArg {
                name: None,
                value: val.node.clone(),
            },
            stoat_config::Arg::Named { name, value } => ResolvedArg {
                name: Some(name.node.clone()),
                value: value.node.clone(),
            },
        })
        .collect();

    ResolvedAction {
        name: action.name.clone(),
        args,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestState {
        values: HashMap<String, StateValue>,
    }

    impl TestState {
        fn new() -> Self {
            Self {
                values: HashMap::new(),
            }
        }

        fn set(mut self, key: &str, val: StateValue) -> Self {
            self.values.insert(key.to_string(), val);
            self
        }
    }

    impl KeymapState for TestState {
        fn get(&self, field: &str) -> Option<&StateValue> {
            self.values.get(field)
        }
    }

    fn key_event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn parse_config(src: &str) -> Config {
        let (config, errors) = stoat_config::parse(src);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        config.expect("expected config")
    }

    #[test]
    fn compile_simple_char() {
        let kp = KeyPart {
            keys: vec![Key::Char('h')],
        };
        let ck = CompiledKey::from_key_part(&kp).expect("should compile");
        assert_eq!(ck.code, KeyCode::Char('h'));
        assert_eq!(ck.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn compile_ctrl_modifier() {
        let kp = KeyPart {
            keys: vec![Key::Char('C'), Key::Char('s')],
        };
        let ck = CompiledKey::from_key_part(&kp).expect("should compile");
        assert_eq!(ck.code, KeyCode::Char('s'));
        assert_eq!(ck.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn compile_shift_modifier() {
        let kp = KeyPart {
            keys: vec![Key::Char('S'), Key::Named("Tab".into())],
        };
        let ck = CompiledKey::from_key_part(&kp).expect("should compile");
        assert_eq!(ck.code, KeyCode::Tab);
        assert_eq!(ck.modifiers, KeyModifiers::SHIFT);
    }

    #[test]
    fn compile_named_keys() {
        for (name, expected) in [
            ("Space", KeyCode::Char(' ')),
            ("Escape", KeyCode::Esc),
            ("Enter", KeyCode::Enter),
            ("Tab", KeyCode::Tab),
            ("F1", KeyCode::F(1)),
            ("F12", KeyCode::F(12)),
        ] {
            let kp = KeyPart {
                keys: vec![Key::Named(name.into())],
            };
            let ck = CompiledKey::from_key_part(&kp).expect("should compile");
            assert_eq!(ck.code, expected, "failed for {name}");
        }
    }

    #[test]
    fn compile_uppercase_char() {
        let kp = KeyPart {
            keys: vec![Key::Char('G')],
        };
        let ck = CompiledKey::from_key_part(&kp).expect("should compile");
        assert_eq!(ck.code, KeyCode::Char('G'));
        assert_eq!(ck.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn matches_event() {
        let ck = CompiledKey {
            code: KeyCode::Char('s'),
            modifiers: KeyModifiers::CONTROL,
        };
        assert!(ck.matches(&key_event(KeyCode::Char('s'), KeyModifiers::CONTROL)));
        assert!(!ck.matches(&key_event(KeyCode::Char('s'), KeyModifiers::NONE)));
        assert!(!ck.matches(&key_event(KeyCode::Char('x'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn eval_eq_string() {
        let config = parse_config(r#"on key { mode == "normal" { q -> Quit(); } }"#);
        let block = match &config.blocks[0].node.statements[0].node {
            Statement::PredicateBlock(b) => b,
            _ => panic!("expected predicate block"),
        };
        let state = TestState::new().set("mode", StateValue::String("normal".into()));
        assert!(evaluate(&block.predicate.node, &state));

        let state = TestState::new().set("mode", StateValue::String("insert".into()));
        assert!(!evaluate(&block.predicate.node, &state));
    }

    #[test]
    fn chord_for_action_reconstructs_mode_prefixes() {
        let config = parse_config(
            r#"on key {
                mode == normal {
                    : -> OpenCommandPalette();
                    a -> Increment();
                    x -> Yank();
                    x -> Yank();
                    Space -> SetMode(space);
                    z -> SetMode(z);
                }
                mode == space {
                    p -> OpenFileFinder();
                    i -> Increment();
                    b -> SetMode(space_buffer);
                }
                mode == space_buffer { n -> OpenBufferPicker(); }
                mode == z { c -> [FoldAtCursor(), SetMode(normal)]; }
            }"#,
        );
        let keymap = Keymap::compile(&config);

        assert_eq!(
            keymap.chord_for_action("OpenCommandPalette").as_deref(),
            Some(":"),
            "direct normal-mode binding has no prefix"
        );
        assert_eq!(
            keymap.chord_for_action("OpenFileFinder").as_deref(),
            Some("Spc p"),
            "leader binding reconstructs the Space prefix"
        );
        assert_eq!(
            keymap.chord_for_action("OpenBufferPicker").as_deref(),
            Some("Spc b n"),
            "nested submode chains both prefixes"
        );
        assert_eq!(
            keymap.chord_for_action("FoldAtCursor").as_deref(),
            Some("z c"),
            "submode action keeps its entry key"
        );
        assert_eq!(keymap.chord_for_action("Unbound"), None);

        assert_eq!(
            keymap.chords_for_action("Increment"),
            ["a", "Spc i"],
            "every chord, in config order"
        );
        assert_eq!(
            keymap.chord_for_action("Increment").as_deref(),
            Some("a"),
            "chord_for_action is the first of chords_for_action"
        );
        assert_eq!(
            keymap.chords_for_action("Yank"),
            ["x"],
            "identical labels are deduped"
        );
        assert_eq!(
            keymap.chords_for_action("OpenBufferPicker"),
            ["Spc b n"],
            "a singly-bound nested action yields a one-element list"
        );
        assert!(keymap.chords_for_action("Unbound").is_empty());
    }

    #[test]
    fn eval_eq_ident() {
        let config = parse_config("on key { mode == normal { q -> Quit(); } }");
        let block = match &config.blocks[0].node.statements[0].node {
            Statement::PredicateBlock(b) => b,
            _ => panic!("expected predicate block"),
        };
        let state = TestState::new().set("mode", StateValue::String("normal".into()));
        assert!(evaluate(&block.predicate.node, &state));
    }

    #[test]
    fn eval_neq() {
        let config = parse_config(r#"on key { mode != "insert" { q -> Quit(); } }"#);
        let block = match &config.blocks[0].node.statements[0].node {
            Statement::PredicateBlock(b) => b,
            _ => panic!("expected predicate block"),
        };
        let state = TestState::new().set("mode", StateValue::String("normal".into()));
        assert!(evaluate(&block.predicate.node, &state));

        let state = TestState::new().set("mode", StateValue::String("insert".into()));
        assert!(!evaluate(&block.predicate.node, &state));
    }

    #[test]
    fn eval_bool() {
        let config = parse_config("on key { has_selection { d -> Cut(); } }");
        let block = match &config.blocks[0].node.statements[0].node {
            Statement::PredicateBlock(b) => b,
            _ => panic!("expected predicate block"),
        };

        let state = TestState::new().set("has_selection", StateValue::Bool(true));
        assert!(evaluate(&block.predicate.node, &state));

        let state = TestState::new().set("has_selection", StateValue::Bool(false));
        assert!(!evaluate(&block.predicate.node, &state));

        let state = TestState::new();
        assert!(!evaluate(&block.predicate.node, &state));
    }

    #[test]
    fn eval_and() {
        let config =
            parse_config(r#"on key { mode == "normal" && has_selection { d -> Delete(); } }"#);
        let block = match &config.blocks[0].node.statements[0].node {
            Statement::PredicateBlock(b) => b,
            _ => panic!("expected predicate block"),
        };

        let state = TestState::new()
            .set("mode", StateValue::String("normal".into()))
            .set("has_selection", StateValue::Bool(true));
        assert!(evaluate(&block.predicate.node, &state));

        let state = TestState::new()
            .set("mode", StateValue::String("normal".into()))
            .set("has_selection", StateValue::Bool(false));
        assert!(!evaluate(&block.predicate.node, &state));
    }

    #[test]
    fn eval_or() {
        let config =
            parse_config(r#"on key { mode == "normal" || mode == "visual" { y -> Yank(); } }"#);
        let block = match &config.blocks[0].node.statements[0].node {
            Statement::PredicateBlock(b) => b,
            _ => panic!("expected predicate block"),
        };

        let state = TestState::new().set("mode", StateValue::String("normal".into()));
        assert!(evaluate(&block.predicate.node, &state));

        let state = TestState::new().set("mode", StateValue::String("visual".into()));
        assert!(evaluate(&block.predicate.node, &state));

        let state = TestState::new().set("mode", StateValue::String("insert".into()));
        assert!(!evaluate(&block.predicate.node, &state));
    }

    #[test]
    fn eval_numeric_comparison() {
        let config = parse_config("on key { count > 1 { j -> MoveDown(); } }");
        let block = match &config.blocks[0].node.statements[0].node {
            Statement::PredicateBlock(b) => b,
            _ => panic!("expected predicate block"),
        };

        let state = TestState::new().set("count", StateValue::Number(5.0));
        assert!(evaluate(&block.predicate.node, &state));

        let state = TestState::new().set("count", StateValue::Number(1.0));
        assert!(!evaluate(&block.predicate.node, &state));
    }

    #[test]
    fn eval_glob_matches() {
        let config = parse_config(r#"on buffer { path ~ "*.rs" { q -> Quit(); } }"#);
        let block = match &config.blocks[0].node.statements[0].node {
            Statement::PredicateBlock(b) => b,
            _ => panic!("expected predicate block"),
        };

        let state = TestState::new().set("path", StateValue::String("main.rs".into()));
        assert!(evaluate(&block.predicate.node, &state));

        let state = TestState::new().set("path", StateValue::String("main.py".into()));
        assert!(!evaluate(&block.predicate.node, &state));
    }

    #[test]
    fn compile_simple_binding() {
        let config = parse_config("on key { q -> Quit(); }");
        let keymap = Keymap::compile(&config);

        assert_eq!(keymap.bindings.len(), 1);
        assert_eq!(keymap.bindings[0].actions[0].name, "Quit");
        assert!(keymap.bindings[0].predicates.is_empty());
    }

    #[test]
    fn compile_with_predicate() {
        let config = parse_config(r#"on key { mode == "normal" { q -> Quit(); } }"#);
        let keymap = Keymap::compile(&config);

        assert_eq!(keymap.bindings.len(), 1);
        assert_eq!(keymap.bindings[0].predicates.len(), 1);
        assert_eq!(keymap.bindings[0].actions[0].name, "Quit");
    }

    #[test]
    fn compile_nested_predicates() {
        let config = parse_config(
            r#"on key {
                mode == "normal" {
                    has_selection {
                        d -> Delete();
                    }
                }
            }"#,
        );
        let keymap = Keymap::compile(&config);

        assert_eq!(keymap.bindings.len(), 1);
        assert_eq!(keymap.bindings[0].predicates.len(), 2);
    }

    #[test]
    fn compile_fn_expansion() {
        let config = parse_config(
            r#"on key {
                fn motions() {
                    h -> MoveLeft();
                    l -> MoveRight();
                }
                mode == "normal" {
                    motions();
                }
            }"#,
        );
        let keymap = Keymap::compile(&config);

        assert_eq!(keymap.bindings.len(), 2);
        assert_eq!(keymap.bindings[0].actions[0].name, "MoveLeft");
        assert_eq!(keymap.bindings[1].actions[0].name, "MoveRight");
        assert_eq!(keymap.bindings[0].predicates.len(), 1);
    }

    #[test]
    fn lookup_returns_first_match() {
        let config = parse_config(
            r#"on key {
                mode == "normal" { q -> Quit(); }
                q -> Default();
            }"#,
        );
        let keymap = Keymap::compile(&config);
        let state = TestState::new().set("mode", StateValue::String("normal".into()));
        let event = key_event(KeyCode::Char('q'), KeyModifiers::NONE);

        let actions = keymap.lookup(&state, &event).expect("should match");
        assert_eq!(actions[0].name, "Quit");
    }

    #[test]
    fn question_mark_opens_help_outside_input_modes() {
        let config = parse_config(crate::DEFAULT_KEYMAP);
        let keymap = Keymap::compile(&config);
        let event = key_event(KeyCode::Char('?'), KeyModifiers::NONE);

        for mode in [
            "normal",
            "review",
            "rebase",
            "commits",
            "conflict",
            "line_select",
            "space",
            "project_tree",
        ] {
            let state = TestState::new()
                .set("mode", StateValue::String(mode.into()))
                .set("input_active", StateValue::Bool(false));
            let actions = keymap.lookup(&state, &event).expect("? should resolve");
            assert_eq!(actions[0].name, "OpenHelp", "mode {mode}");
        }

        for mode in ["insert", "prompt", "run", "reword_insert"] {
            let state = TestState::new()
                .set("mode", StateValue::String(mode.into()))
                .set("input_active", StateValue::Bool(true));
            let resolved = keymap.lookup(&state, &event).map(|a| a[0].name.as_str());
            assert_ne!(resolved, Some("OpenHelp"), "mode {mode}");
        }
    }

    #[test]
    fn ctrl_equals_and_ctrl_minus_adjust_font_size_in_every_mode() {
        let config = parse_config(crate::DEFAULT_KEYMAP);
        let keymap = Keymap::compile(&config);

        let states = [
            TestState::new()
                .set("mode", StateValue::String("normal".into()))
                .set("input_active", StateValue::Bool(false)),
            TestState::new()
                .set("mode", StateValue::String("insert".into()))
                .set("input_active", StateValue::Bool(true)),
            TestState::new()
                .set("mode", StateValue::String("prompt".into()))
                .set("input_active", StateValue::Bool(true))
                .set("finder_open", StateValue::Bool(true)),
        ];

        for state in states {
            let increase = keymap
                .lookup(
                    &state,
                    &key_event(KeyCode::Char('='), KeyModifiers::CONTROL),
                )
                .expect("Ctrl-= should resolve in every mode");
            assert_eq!(increase[0].name, "IncreaseFontSize");

            let decrease = keymap
                .lookup(
                    &state,
                    &key_event(KeyCode::Char('-'), KeyModifiers::CONTROL),
                )
                .expect("Ctrl-Minus should resolve in every mode");
            assert_eq!(decrease[0].name, "DecreaseFontSize");
        }
    }

    #[test]
    fn lookup_falls_through() {
        let config = parse_config(
            r#"on key {
                mode == "insert" { q -> InsertChar(); }
                q -> Quit();
            }"#,
        );
        let keymap = Keymap::compile(&config);
        let state = TestState::new().set("mode", StateValue::String("normal".into()));
        let event = key_event(KeyCode::Char('q'), KeyModifiers::NONE);

        let actions = keymap.lookup(&state, &event).expect("should fall through");
        assert_eq!(actions[0].name, "Quit");
    }

    #[test]
    fn lookup_no_match() {
        let config = parse_config(r#"on key { mode == "normal" { q -> Quit(); } }"#);
        let keymap = Keymap::compile(&config);
        let state = TestState::new().set("mode", StateValue::String("normal".into()));
        let event = key_event(KeyCode::Char('x'), KeyModifiers::NONE);

        assert!(keymap.lookup(&state, &event).is_none());
    }

    #[test]
    fn lookup_with_action_args() {
        let config = parse_config("on key { h -> MoveCursor(direction: left, count: 1); }");
        let keymap = Keymap::compile(&config);
        let state = TestState::new();
        let event = key_event(KeyCode::Char('h'), KeyModifiers::NONE);

        let actions = keymap.lookup(&state, &event).expect("should match");
        assert_eq!(actions[0].name, "MoveCursor");
        assert_eq!(actions[0].args.len(), 2);
        assert_eq!(actions[0].args[0].name.as_deref(), Some("direction"));
        assert_eq!(actions[0].args[1].name.as_deref(), Some("count"));
    }

    #[test]
    fn lookup_action_sequence() {
        let config = parse_config("on key { C-k -> [SelectLine(), Comment()]; }");
        let keymap = Keymap::compile(&config);
        let state = TestState::new();
        let event = key_event(KeyCode::Char('k'), KeyModifiers::CONTROL);

        let actions = keymap.lookup(&state, &event).expect("should match");
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].name, "SelectLine");
        assert_eq!(actions[1].name, "Comment");
    }

    #[test]
    fn full_keymap_round_trip() {
        let config = parse_config(
            r#"
            on key {
                mode == "normal" {
                    q -> Quit();
                    Escape -> Quit();
                    Space -> SetMode(space);
                }
                mode == "space" {
                    a -> SetMode(space_a);
                    Escape -> SetMode(normal);
                }
                mode == "space_a" {
                    s -> SplitRight();
                    Escape -> SetMode(normal);
                }
            }
            "#,
        );
        let keymap = Keymap::compile(&config);

        let state = TestState::new().set("mode", StateValue::String("normal".into()));
        let actions = keymap
            .lookup(&state, &key_event(KeyCode::Char('q'), KeyModifiers::NONE))
            .expect("should match");
        assert_eq!(actions[0].name, "Quit");

        let actions = keymap
            .lookup(&state, &key_event(KeyCode::Char(' '), KeyModifiers::NONE))
            .expect("should match");
        assert_eq!(actions[0].name, "SetMode");

        let state = TestState::new().set("mode", StateValue::String("space".into()));
        let actions = keymap
            .lookup(&state, &key_event(KeyCode::Char('a'), KeyModifiers::NONE))
            .expect("should match");
        assert_eq!(actions[0].name, "SetMode");

        let state = TestState::new().set("mode", StateValue::String("space_a".into()));
        let actions = keymap
            .lookup(&state, &key_event(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("should match");
        assert_eq!(actions[0].name, "SplitRight");

        assert!(keymap
            .lookup(&state, &key_event(KeyCode::Char('q'), KeyModifiers::NONE))
            .is_none());
    }

    #[test]
    fn ignores_non_key_blocks() {
        let config = parse_config(
            r#"
            on init { font.size = 14; }
            on key { q -> Quit(); }
            "#,
        );
        let keymap = Keymap::compile(&config);
        assert_eq!(keymap.bindings.len(), 1);
    }

    #[test]
    fn display_label_char() {
        let ck = CompiledKey {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(ck.display_label(), "q");
    }

    #[test]
    fn display_label_ctrl() {
        let ck = CompiledKey {
            code: KeyCode::Char('s'),
            modifiers: KeyModifiers::CONTROL,
        };
        assert_eq!(ck.display_label(), "C-s");
    }

    #[test]
    fn display_label_named() {
        assert_eq!(
            CompiledKey {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE
            }
            .display_label(),
            "Esc"
        );
        assert_eq!(
            CompiledKey {
                code: KeyCode::Char(' '),
                modifiers: KeyModifiers::NONE
            }
            .display_label(),
            "Spc"
        );
    }

    #[test]
    fn display_label_backtab() {
        let ck = CompiledKey {
            code: KeyCode::BackTab,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(ck.display_label(), "S-Tab");
    }

    #[test]
    fn to_key_token_char() {
        let ck = CompiledKey {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(ck.to_key_token(), "q");
    }

    #[test]
    fn to_key_token_space() {
        let ck = CompiledKey {
            code: KeyCode::Char(' '),
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(ck.to_key_token(), "space");
    }

    #[test]
    fn to_key_token_ctrl() {
        let ck = CompiledKey {
            code: KeyCode::Char('s'),
            modifiers: KeyModifiers::CONTROL,
        };
        assert_eq!(ck.to_key_token(), "ctrl-s");
    }

    #[test]
    fn active_bindings_returns_matches() {
        let config = parse_config(
            r#"on key {
                mode == "space" {
                    q -> Quit();
                    a -> SetMode(space_a);
                }
                mode == "normal" {
                    j -> MoveDown();
                }
            }"#,
        );
        let keymap = Keymap::compile(&config);
        let state = TestState::new().set("mode", StateValue::String("space".into()));
        let bindings = keymap.active_bindings(&state);

        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].0, "q");
        assert_eq!(bindings[0].1[0].name, "Quit");
        assert_eq!(bindings[1].0, "a");
        assert_eq!(bindings[1].1[0].name, "SetMode");
    }

    #[test]
    fn active_bindings_excludes_non_matching() {
        let config = parse_config(
            r#"on key {
                mode == "space" { q -> Quit(); }
                mode == "normal" { j -> MoveDown(); }
            }"#,
        );
        let keymap = Keymap::compile(&config);

        let space = TestState::new().set("mode", StateValue::String("space".into()));
        assert_eq!(keymap.active_bindings(&space).len(), 1);

        let normal = TestState::new().set("mode", StateValue::String("normal".into()));
        assert_eq!(keymap.active_bindings(&normal).len(), 1);

        let insert = TestState::new().set("mode", StateValue::String("insert".into()));
        assert!(keymap.active_bindings(&insert).is_empty());
    }

    #[test]
    fn active_bindings_compound_predicates() {
        let config = parse_config(
            r#"on key {
                mode == "space" && has_selection {
                    d -> Delete();
                }
                mode == "space" {
                    q -> Quit();
                }
            }"#,
        );
        let keymap = Keymap::compile(&config);

        let without = TestState::new().set("mode", StateValue::String("space".into()));
        assert_eq!(keymap.active_bindings(&without).len(), 1);
        assert_eq!(keymap.active_bindings(&without)[0].1[0].name, "Quit");

        let with = TestState::new()
            .set("mode", StateValue::String("space".into()))
            .set("has_selection", StateValue::Bool(true));
        assert_eq!(keymap.active_bindings(&with).len(), 2);
    }

    #[test]
    fn scoped_bindings_keeps_only_scoped() {
        let config = parse_config(
            r#"on key {
                ? -> OpenHelp();
                mode == "normal" && help_open {
                    Escape -> CloseHelp();
                    j -> HelpNext();
                }
                mode == "normal" {
                    q -> Quit();
                    j -> MoveDown();
                }
            }"#,
        );
        let keymap = Keymap::compile(&config);

        let state = TestState::new()
            .set("mode", StateValue::String("normal".into()))
            .set("help_open", StateValue::Bool(true));

        let scoped: Vec<_> = keymap
            .scoped_bindings(&state, "help_open")
            .iter()
            .map(|(k, a)| (k.clone(), a[0].name.clone()))
            .collect();
        assert_eq!(
            scoped,
            vec![
                ("Esc".to_string(), "CloseHelp".to_string()),
                ("j".to_string(), "HelpNext".to_string()),
            ]
        );
    }

    #[test]
    fn scoped_bindings_requires_state_match() {
        let config = parse_config(
            r#"on key {
                mode == "normal" && help_open {
                    Escape -> CloseHelp();
                }
            }"#,
        );
        let keymap = Keymap::compile(&config);

        let closed = TestState::new()
            .set("mode", StateValue::String("normal".into()))
            .set("help_open", StateValue::Bool(false));
        assert!(keymap.scoped_bindings(&closed, "help_open").is_empty());

        let wrong_mode = TestState::new()
            .set("mode", StateValue::String("insert".into()))
            .set("help_open", StateValue::Bool(true));
        assert!(keymap.scoped_bindings(&wrong_mode, "help_open").is_empty());
    }

    #[test]
    fn scoped_bindings_ignores_unrelated_scope() {
        let config = parse_config(
            r#"on key {
                mode == "normal" && palette_open {
                    Escape -> ClosePalette();
                }
                mode == "normal" && help_open {
                    Escape -> CloseHelp();
                }
            }"#,
        );
        let keymap = Keymap::compile(&config);

        let state = TestState::new()
            .set("mode", StateValue::String("normal".into()))
            .set("help_open", StateValue::Bool(true))
            .set("palette_open", StateValue::Bool(true));

        let scoped: Vec<_> = keymap
            .scoped_bindings(&state, "help_open")
            .iter()
            .map(|(_, a)| a[0].name.clone())
            .collect();
        assert_eq!(scoped, vec!["CloseHelp".to_string()]);
    }
}
