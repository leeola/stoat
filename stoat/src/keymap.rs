use compact_str::CompactString;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
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
}

fn resolve_key(key: &Key) -> Option<KeyCode> {
    match key {
        Key::Char(c) => Some(KeyCode::Char(*c)),
        Key::Named(name) => match name.as_str() {
            "Space" => Some(KeyCode::Char(' ')),
            "Escape" | "Esc" => Some(KeyCode::Esc),
            "Enter" | "Return" => Some(KeyCode::Enter),
            "Tab" => Some(KeyCode::Tab),
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
        (Some(StateValue::Number(n)), Value::Number(v)) => {
            n.partial_cmp(v).map_or(false, |o| check(o))
        },
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

#[derive(Debug, Clone)]
pub struct ResolvedAction {
    pub name: String,
    pub args: Vec<ResolvedArg>,
}

#[derive(Debug, Clone)]
pub struct ResolvedArg {
    pub name: Option<String>,
    pub value: Value,
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

fn resolve_config_action(action: &stoat_config::Action) -> ResolvedAction {
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
}
