use compact_str::CompactString;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use stoat_config::{
    ActionExpr, Binding, Config, EventType, Key, KeyPart, Predicate, Statement, Value,
};

#[derive(Debug, Clone, PartialEq)]
pub enum StateValue {
    String(CompactString),
    Number(f64),
    Bool(bool),
}

pub trait KeymapState {
    fn get(&self, field: &str) -> Option<&StateValue>;
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
        Predicate::Not(p) => !evaluate(&p.node, state),
        Predicate::And(l, r) => evaluate(&l.node, state) && evaluate(&r.node, state),
        Predicate::Or(l, r) => evaluate(&l.node, state) || evaluate(&r.node, state),
    }
}

/// The count of leaf predicate atoms in `predicate`, the specificity a binding
/// contributes per predicate when resolving competing bindings.
///
/// An `And` sums its branches, since satisfying it requires all of them. Every
/// other form counts as one. Leaves are a single atom, an `Or` scores one
/// because it is satisfiable by its weakest branch, and a `Not` scores one
/// because a negation is broadly satisfiable and so earns no specificity.
fn predicate_atoms(predicate: &Predicate) -> usize {
    match predicate {
        Predicate::And(l, r) => predicate_atoms(&l.node) + predicate_atoms(&r.node),
        _ => 1,
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

#[derive(Debug, Clone)]
struct CompiledBinding {
    key: CompiledKey,
    predicates: Vec<Predicate>,
    actions: Arc<[ResolvedAction]>,
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

    /// Every compiled binding as `(key, predicates, actions)`, in source order.
    ///
    /// Help walks this to index which keys reach each action and under what
    /// conditions, rather than resolving one key against a single state.
    pub fn bindings(
        &self,
    ) -> impl Iterator<Item = (&CompiledKey, &[Predicate], &[ResolvedAction])> {
        self.bindings.iter().map(|binding| {
            (
                &binding.key,
                binding.predicates.as_slice(),
                &*binding.actions,
            )
        })
    }

    /// Compile `config` and collect warnings for `SetMode` targets no binding
    /// block selects on.
    ///
    /// A mode name is a free ident, so a typo in a `SetMode(mode)` action leaves
    /// the keymap with no way back out of that mode. Each target that no
    /// `mode ==`/`mode !=` predicate mentions is warned, except `normal` and
    /// `insert`, which have hard-coded behavior and stay reachable without a
    /// binding block.
    pub fn compile_with_warnings(config: &Config) -> (Self, Vec<String>) {
        let keymap = Self::compile(config);

        let mut set_mode_targets: HashSet<String> = HashSet::new();
        let mut selected_modes: HashSet<String> = HashSet::new();
        for binding in &keymap.bindings {
            for action in binding.actions.iter() {
                if action.name == "SetMode"
                    && let Some(target) = action.args.first().and_then(|a| value_str(&a.value))
                {
                    set_mode_targets.insert(target.to_string());
                }
            }
            for predicate in &binding.predicates {
                collect_mode_targets(predicate, &mut selected_modes);
            }
        }

        let mut warnings: Vec<String> = set_mode_targets
            .into_iter()
            .filter(|target| target != "normal" && target != "insert")
            .filter(|target| !selected_modes.contains(target))
            .map(|target| format!("SetMode target `{target}` matches no `mode ==` binding block"))
            .collect();
        warnings.sort();

        (keymap, warnings)
    }

    pub fn lookup(
        &self,
        state: &dyn KeymapState,
        event: &KeyEvent,
    ) -> Option<Arc<[ResolvedAction]>> {
        let mut best: Option<(usize, &Arc<[ResolvedAction]>)> = None;
        for binding in &self.bindings {
            if !binding.key.matches(event) {
                continue;
            }
            if !binding.predicates.iter().all(|p| evaluate(p, state)) {
                continue;
            }
            let score: usize = binding.predicates.iter().map(predicate_atoms).sum();
            // Strict `>` keeps the earliest binding on a tie, so equally specific
            // matches still resolve in source order.
            if best.is_none_or(|(best_score, _)| score > best_score) {
                best = Some((score, &binding.actions));
            }
        }
        best.map(|(_, actions)| actions.clone())
    }

    pub fn active_keys(&self, state: &dyn KeymapState) -> Vec<(&CompiledKey, &[ResolvedAction])> {
        let mut results = Vec::new();
        for binding in &self.bindings {
            let matches = binding.predicates.iter().all(|p| evaluate(p, state));
            if matches {
                results.push((&binding.key, binding.actions.as_ref()));
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
                results.push((binding.key.display_label(), binding.actions.as_ref()));
            }
        }
        results
    }

    /// Returns the active bindings scoped to `scope_field == scope_value`. A
    /// binding qualifies when at least one of its predicates compares
    /// `scope_field` equal to `scope_value`.
    ///
    /// Used to surface only the keybinds specific to one open modal (e.g.
    /// `modal == help`) in hint popups. The equality test excludes both the
    /// broader mode-level bindings and the generic `modal`-truthy bindings that
    /// also match while a modal is open, so the popup lists only that modal's
    /// own keys.
    pub fn scoped_bindings(
        &self,
        state: &dyn KeymapState,
        scope_field: &str,
        scope_value: &str,
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
                .any(|p| predicate_eq_matches(p, scope_field, scope_value));
            if in_scope {
                results.push((binding.key.display_label(), binding.actions.as_ref()));
            }
        }
        results
    }
}

fn predicate_eq_matches(pred: &Predicate, field: &str, value: &str) -> bool {
    match pred {
        Predicate::Eq(f, v) => f.node == field && value_str(&v.node) == Some(value),
        Predicate::And(l, r) | Predicate::Or(l, r) => {
            predicate_eq_matches(&l.node, field, value)
                || predicate_eq_matches(&r.node, field, value)
        },
        // A negated scope equality is not a scope claim, so `Not` is not walked.
        Predicate::Not(_) => false,
        _ => false,
    }
}

/// The string a `String` or bare `Ident` config value carries, if any.
fn value_str(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) | Value::Ident(s) => Some(s),
        _ => None,
    }
}

/// Collect the mode names any `mode ==`/`mode !=` predicate selects on, walking
/// through `And`/`Or`.
fn collect_mode_targets(predicate: &Predicate, out: &mut HashSet<String>) {
    match predicate {
        Predicate::Eq(field, val) | Predicate::NotEq(field, val) => {
            if field.node == "mode"
                && let Some(target) = value_str(&val.node)
            {
                out.insert(target.to_string());
            }
        },
        Predicate::And(l, r) | Predicate::Or(l, r) => {
            collect_mode_targets(&l.node, out);
            collect_mode_targets(&r.node, out);
        },
        // `!(mode == foo)` still targets foo, so it is registered to avoid a
        // spurious unknown-mode warning for a mode the config does reference.
        Predicate::Not(p) => collect_mode_targets(&p.node, out),
        _ => {},
    }
}

/// Collect every state field name `predicate` references, walking through
/// `And`/`Or`/`Not`.
///
/// Consumed by the help detail pane to annotate a binding's conditions with the
/// current value of each field they test.
pub(crate) fn collect_predicate_fields(predicate: &Predicate, out: &mut Vec<String>) {
    match predicate {
        Predicate::Eq(field, _)
        | Predicate::NotEq(field, _)
        | Predicate::Gt(field, _)
        | Predicate::Lt(field, _)
        | Predicate::Gte(field, _)
        | Predicate::Lte(field, _)
        | Predicate::Matches(field, _)
        | Predicate::Bool(field) => out.push(field.node.clone()),
        Predicate::Not(p) => collect_predicate_fields(&p.node, out),
        Predicate::And(l, r) | Predicate::Or(l, r) => {
            collect_predicate_fields(&l.node, out);
            collect_predicate_fields(&r.node, out);
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
        actions: actions.into(),
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
    use std::collections::BTreeMap;
    use stoat_config::{LineNumbers, MouseCapturePolicy, Settings, WrapMode};

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

    fn predicate_of(src: &str) -> Predicate {
        let config = parse_config(src);
        match &config.blocks[0].node.statements[0].node {
            Statement::PredicateBlock(b) => b.predicate.node.clone(),
            _ => panic!("expected predicate block"),
        }
    }

    #[test]
    fn eval_not() {
        let not_selection = predicate_of("on key { !has_selection { d -> Cut(); } }");
        let present = TestState::new().set("has_selection", StateValue::Bool(true));
        assert!(
            !evaluate(&not_selection, &present),
            "Not of a true bool is false"
        );
        assert!(
            evaluate(&not_selection, &TestState::new()),
            "Not of an absent field is true",
        );

        let not_normal = predicate_of(r#"on key { !(mode == "normal") { q -> Quit(); } }"#);
        let normal = TestState::new().set("mode", StateValue::String("normal".into()));
        assert!(
            !evaluate(&not_normal, &normal),
            "Not of a matching Eq is false"
        );
        let insert = TestState::new().set("mode", StateValue::String("insert".into()));
        assert!(
            evaluate(&not_normal, &insert),
            "Not of a failing Eq is true"
        );
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
    fn lookup_prefers_more_specific() {
        let config = parse_config(
            r#"on key {
                mode == "normal" { q -> MoveLeft(); }
                mode == "normal" && palette_open { q -> MoveRight(); }
            }"#,
        );
        let keymap = Keymap::compile(&config);
        let state = TestState::new()
            .set("mode", StateValue::String("normal".into()))
            .set("palette_open", StateValue::Bool(true));
        let event = key_event(KeyCode::Char('q'), KeyModifiers::NONE);

        // The later 2-atom binding outscores the earlier 1-atom one.
        let actions = keymap.lookup(&state, &event).expect("should match");
        assert_eq!(actions[0].name, "MoveRight");
    }

    #[test]
    fn lookup_prefers_token_specific_binding_and_falls_back_without_an_index() {
        let config = parse_config(
            r#"on key {
                mode == "space_lsp" { c -> MoveLeft(); }
                mode == "space_lsp" && token == "function" { c -> MoveRight(); }
            }"#,
        );
        let keymap = Keymap::compile(&config);
        let event = key_event(KeyCode::Char('c'), KeyModifiers::NONE);

        // Over a function token the 2-atom binding wins.
        let over_function = TestState::new()
            .set("mode", StateValue::String("space_lsp".into()))
            .set("token_known", StateValue::Bool(true))
            .set("token", StateValue::String("function".into()));
        assert_eq!(
            keymap.lookup(&over_function, &event).expect("match")[0].name,
            "MoveRight",
        );

        // With no index `token` is absent, so the token binding fails and the
        // base binding wins.
        let no_index = TestState::new()
            .set("mode", StateValue::String("space_lsp".into()))
            .set("token_known", StateValue::Bool(false));
        assert_eq!(
            keymap.lookup(&no_index, &event).expect("match")[0].name,
            "MoveLeft",
        );
    }

    #[test]
    fn space_lsp_token_bindings_gate_by_cursor_kind() {
        let config = parse_config(
            r#"on key {
                mode == space_lsp {
                    !token_known || token == function { c -> GotoCaller(); }
                    !token_known || token == trait { T -> GotoImplementors(); }
                }
            }"#,
        );
        let keymap = Keymap::compile(&config);
        let c = key_event(KeyCode::Char('c'), KeyModifiers::NONE);
        let t = key_event(KeyCode::Char('T'), KeyModifiers::NONE);

        let over_trait = TestState::new()
            .set("mode", StateValue::String("space_lsp".into()))
            .set("token_known", StateValue::Bool(true))
            .set("token", StateValue::String("trait".into()));
        assert!(
            keymap.lookup(&over_trait, &c).is_none(),
            "the caller binding is unbound over a trait",
        );
        assert_eq!(
            keymap
                .lookup(&over_trait, &t)
                .expect("T binds over a trait")[0]
                .name,
            "GotoImplementors",
        );

        let over_function = TestState::new()
            .set("mode", StateValue::String("space_lsp".into()))
            .set("token_known", StateValue::Bool(true))
            .set("token", StateValue::String("function".into()));
        assert_eq!(
            keymap
                .lookup(&over_function, &c)
                .expect("c binds over a function")[0]
                .name,
            "GotoCaller",
        );
        assert!(
            keymap.lookup(&over_function, &t).is_none(),
            "the implementors binding is unbound over a function",
        );
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
    fn lookup_ties_keep_source_order() {
        let config = parse_config(
            r#"on key {
                mode == "normal" { q -> MoveLeft(); }
                mode != "insert" { q -> MoveRight(); }
            }"#,
        );
        let keymap = Keymap::compile(&config);
        let state = TestState::new().set("mode", StateValue::String("normal".into()));
        let event = key_event(KeyCode::Char('q'), KeyModifiers::NONE);

        // Both bindings match with one atom each, so the earlier one wins.
        let actions = keymap.lookup(&state, &event).expect("should match");
        assert_eq!(actions[0].name, "MoveLeft");
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
    fn compile_warns_on_unreachable_set_mode_target() {
        let config = parse_config("on key { i -> SetMode(nomral); }");
        let (_, warnings) = Keymap::compile_with_warnings(&config);
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(
            warnings[0].contains("nomral"),
            "warning names the ident: {}",
            warnings[0]
        );
    }

    #[test]
    fn compile_no_warning_for_reachable_targets() {
        let config = parse_config(
            r#"on key {
                mode == "diff" { j -> Quit(); }
                i -> SetMode(diff);
                n -> SetMode(normal);
            }"#,
        );
        let (_, warnings) = Keymap::compile_with_warnings(&config);
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
    }

    #[test]
    fn default_config_compiles_without_warnings() {
        let config = parse_config(crate::app::DEFAULT_KEYMAP);
        let (_, warnings) = Keymap::compile_with_warnings(&config);
        assert!(warnings.is_empty(), "shipped config warnings: {warnings:?}");
    }

    #[test]
    fn question_mark_rebinds_across_modes() {
        let config = parse_config(crate::app::DEFAULT_KEYMAP);
        let keymap = Keymap::compile(&config);
        let question = key_event(KeyCode::Char('?'), KeyModifiers::NONE);
        let name_in = |mode: &str| {
            let state = TestState::new().set("mode", StateValue::String(mode.into()));
            keymap
                .lookup(&state, &question)
                .unwrap_or_else(|| panic!("? is unbound in {mode}"))[0]
                .name
                .clone()
        };

        assert_eq!(
            name_in("normal"),
            "OpenHelp",
            "? opens help directly in normal mode",
        );
        assert_eq!(
            name_in("goto"),
            "OpenReverseSearchInput",
            "reverse search relocated to g ?",
        );
        assert_eq!(
            name_in("space"),
            "ToggleKeyHints",
            "the hints toggle relocated to the space leader",
        );
        assert_eq!(
            name_in("space_buffer"),
            "OpenHelp",
            "? still opens help in other sub-modes",
        );
    }

    #[test]
    fn space_pane_display_binds_digits_to_focus_pane() {
        let config = parse_config(crate::app::DEFAULT_KEYMAP);
        let keymap = Keymap::compile(&config);

        let nav = TestState::new().set("mode", StateValue::String("space_pane_nav".into()));
        let enter = keymap
            .lookup(&nav, &key_event(KeyCode::Char('e'), KeyModifiers::NONE))
            .expect("e is bound in space_pane_nav");
        assert_eq!(enter[0].name, "SetMode");
        assert_eq!(
            enter[0].args[0].value,
            Value::Ident("space_pane_display".into())
        );

        let display = TestState::new().set("mode", StateValue::String("space_pane_display".into()));
        let three = keymap
            .lookup(&display, &key_event(KeyCode::Char('3'), KeyModifiers::NONE))
            .expect("3 is bound in space_pane_display");
        assert_eq!(three[0].name, "FocusPane");
        assert_eq!(three[0].args[0].value, Value::Number(3.0));
        assert_eq!(three[1].name, "SetMode");

        let ten = keymap
            .lookup(&display, &key_event(KeyCode::Char('0'), KeyModifiers::NONE))
            .expect("0 is bound in space_pane_display");
        assert_eq!(
            ten[0].args[0].value,
            Value::Number(10.0),
            "0 focuses pane 10"
        );
    }

    #[test]
    fn diff_view_does_not_shadow_normal_mode_keys() {
        let config = parse_config(crate::app::DEFAULT_KEYMAP);
        let keymap = Keymap::compile(&config);

        let diff = TestState::new()
            .set("view", StateValue::String("diff".into()))
            .set("mode", StateValue::String("normal".into()));
        let m = keymap
            .lookup(&diff, &key_event(KeyCode::Char('m'), KeyModifiers::NONE))
            .expect("m falls through to the normal-mode match binding");
        assert_eq!(m[0].name, "SetMode");
        assert_eq!(m[0].args[0].value, Value::Ident("match".into()));
        assert!(
            keymap
                .lookup(&diff, &key_event(KeyCode::Char('M'), KeyModifiers::NONE))
                .is_none(),
            "M is unbound in the diff view, matching plain normal mode",
        );
    }

    #[test]
    fn space_buffer_binds_d_to_close_buffer() {
        let config = parse_config(crate::app::DEFAULT_KEYMAP);
        let keymap = Keymap::compile(&config);

        let space_buffer = TestState::new().set("mode", StateValue::String("space_buffer".into()));
        let d = keymap
            .lookup(
                &space_buffer,
                &key_event(KeyCode::Char('d'), KeyModifiers::NONE),
            )
            .expect("d is bound in space_buffer");
        assert_eq!(d[0].name, "CloseBuffer");
        assert_eq!(d[1].name, "SetMode");
    }

    #[test]
    fn space_buffer_binds_f_to_auto_reload_follow() {
        let config = parse_config(crate::app::DEFAULT_KEYMAP);
        let keymap = Keymap::compile(&config);

        let space_buffer = TestState::new().set("mode", StateValue::String("space_buffer".into()));
        let f = keymap
            .lookup(
                &space_buffer,
                &key_event(KeyCode::Char('f'), KeyModifiers::NONE),
            )
            .expect("f is bound in space_buffer");
        assert_eq!(f[0].name, "AutoReload");
        assert_eq!(f[0].args[0].value, Value::Ident("follow".into()));
        assert_eq!(f[1].name, "SetMode");
    }

    #[test]
    fn default_config_pins_every_setting_default() {
        let config = parse_config(crate::app::DEFAULT_KEYMAP);
        // The embedded config must actively set every fixed-default scalar, so
        // this asserts the full resolved state. Adding a Settings field breaks
        // the literal, forcing a matching default entry in config.stcfg.
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: Some(false),
                format_on_save: Some(false),
                review_follow: Some(true),
                review_rebase_head: Some(true),
                review_precompute: Some(true),
                theme: Some("default_dark".to_string()),
                mouse_capture: Some(MouseCapturePolicy::Auto),
                scrolloff: Some(3),
                editor_line_numbers: Some(LineNumbers::Relative),
                editor_minimap: None,
                editor_wrap: Some(WrapMode::EditorWidth),
                editor_wrap_column: None,
                ui_inactive_dim: None,
                highlight_retention: Some(64),
                terminal_shell: None,
                terminal_args: None,
                direnv_load: Some(true),
                direnv_reload_on_cd: Some(true),
                direnv_unset_on_exit: Some(false),
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
                lsp_server_lists: BTreeMap::new(),
                lsp_commands: BTreeMap::new(),
                lsp_only: BTreeMap::new(),
                lsp_except: BTreeMap::new(),
                finder_scopes: BTreeMap::new(),
                finder_default_scope: Some("all".to_string()),
            },
        );
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
                modal == "help" {
                    Escape -> CloseHelp();
                    j -> HelpNext();
                }
                modal && mode == "normal" {
                    k -> ModalGeneric();
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
            .set("modal", StateValue::String("help".into()));

        let scoped: Vec<_> = keymap
            .scoped_bindings(&state, "modal", "help")
            .iter()
            .map(|(k, a)| (k.clone(), a[0].name.clone()))
            .collect();
        assert_eq!(
            scoped,
            vec![
                ("Esc".to_string(), "CloseHelp".to_string()),
                ("j".to_string(), "HelpNext".to_string()),
            ],
            "the generic `modal` and bare `mode` bindings must be excluded"
        );
    }

    #[test]
    fn scoped_bindings_requires_state_match() {
        let config = parse_config(
            r#"on key {
                modal == "help" && mode == "normal" {
                    Escape -> CloseHelp();
                }
            }"#,
        );
        let keymap = Keymap::compile(&config);

        let other_modal = TestState::new()
            .set("mode", StateValue::String("normal".into()))
            .set("modal", StateValue::String("palette".into()));
        assert!(keymap
            .scoped_bindings(&other_modal, "modal", "help")
            .is_empty());

        let wrong_mode = TestState::new()
            .set("mode", StateValue::String("insert".into()))
            .set("modal", StateValue::String("help".into()));
        assert!(keymap
            .scoped_bindings(&wrong_mode, "modal", "help")
            .is_empty());
    }

    #[test]
    fn scoped_bindings_ignores_unrelated_scope() {
        let config = parse_config(
            r#"on key {
                modal == "palette" {
                    Escape -> ClosePalette();
                }
                modal == "help" {
                    Escape -> CloseHelp();
                }
            }"#,
        );
        let keymap = Keymap::compile(&config);

        let state = TestState::new()
            .set("mode", StateValue::String("normal".into()))
            .set("modal", StateValue::String("help".into()));

        let scoped: Vec<_> = keymap
            .scoped_bindings(&state, "modal", "help")
            .iter()
            .map(|(_, a)| a[0].name.clone())
            .collect();
        assert_eq!(scoped, vec!["CloseHelp".to_string()]);
    }
}
