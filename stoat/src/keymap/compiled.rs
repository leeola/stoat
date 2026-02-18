use std::collections::HashMap;
use stoat_config::{
    Action, ActionExpr, Arg, Config, EventType, Key, KeyPart, Predicate, Spanned, Statement, Value,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompiledKey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub cmd: bool,
    pub key: Key,
}

pub struct CompiledBinding {
    pub key: CompiledKey,
    pub predicates: Vec<Predicate>,
    pub action: ActionExpr,
}

pub struct CompiledKeymap {
    pub bindings: Vec<CompiledBinding>,
}

fn is_ctrl_modifier(key: &Key) -> bool {
    match key {
        Key::Char('C') => true,
        Key::Named(s) => s == "ctrl" || s == "Ctrl",
        _ => false,
    }
}

fn is_alt_modifier(key: &Key) -> bool {
    match key {
        Key::Char('M') => true,
        Key::Named(s) => s == "alt" || s == "Alt",
        _ => false,
    }
}

fn is_shift_modifier(key: &Key) -> bool {
    match key {
        Key::Char('S') => true,
        Key::Named(s) => s == "shift" || s == "Shift",
        _ => false,
    }
}

fn is_cmd_modifier(key: &Key) -> bool {
    match key {
        Key::Named(s) => s == "cmd" || s == "Cmd",
        _ => false,
    }
}

impl CompiledKey {
    pub fn from_key_part(key_part: &KeyPart) -> Self {
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut cmd = false;
        let mut main_key: Option<Key> = None;

        let keys = &key_part.keys;
        for (i, key) in keys.iter().enumerate() {
            let is_last = i == keys.len() - 1;

            if !is_last {
                if is_ctrl_modifier(key) {
                    ctrl = true;
                    continue;
                } else if is_alt_modifier(key) {
                    alt = true;
                    continue;
                } else if is_shift_modifier(key) {
                    shift = true;
                    continue;
                } else if is_cmd_modifier(key) {
                    cmd = true;
                    continue;
                }
            }

            main_key = Some(key.clone());
        }

        let mut key = main_key.expect("key_part must have at least one key");

        // Normalize Named keys to lowercase
        if let Key::Named(ref s) = key {
            key = Key::Named(s.to_lowercase());
        }

        // Shift+letter normalization: shift + lowercase letter becomes uppercase letter
        if shift {
            if let Key::Char(c) = key {
                if c.is_ascii_lowercase() {
                    key = Key::Char(c.to_ascii_uppercase());
                    shift = false;
                }
            }
        }

        Self {
            ctrl,
            alt,
            shift,
            cmd,
            key,
        }
    }

    pub fn from_keystroke(keystroke: &gpui::Keystroke) -> Self {
        let ctrl = keystroke.modifiers.control;
        let alt = keystroke.modifiers.alt;
        let mut shift = keystroke.modifiers.shift;
        let cmd = keystroke.modifiers.platform;

        let key_str = &keystroke.key;
        let mut key = if key_str.chars().count() == 1 {
            Key::Char(key_str.chars().next().unwrap())
        } else {
            Key::Named(key_str.to_lowercase())
        };

        // Shift+letter normalization
        if shift {
            if let Key::Char(c) = key {
                if c.is_ascii_lowercase() {
                    key = Key::Char(c.to_ascii_uppercase());
                    shift = false;
                }
            }
        }

        Self {
            ctrl,
            alt,
            shift,
            cmd,
            key,
        }
    }

    pub fn to_keystroke(&self) -> gpui::Keystroke {
        let (key_str, key_char) = match &self.key {
            Key::Char(c) => {
                let s = c.to_string();
                (s.clone(), Some(s))
            },
            Key::Named(name) => {
                let kc = match name.as_str() {
                    "enter" => Some("\n".into()),
                    "space" => Some(" ".into()),
                    "tab" => Some("\t".into()),
                    _ => None,
                };
                (name.clone(), kc)
            },
        };
        gpui::Keystroke {
            key: key_str,
            modifiers: gpui::Modifiers {
                control: self.ctrl,
                alt: self.alt,
                shift: self.shift,
                platform: self.cmd,
                ..Default::default()
            },
            key_char,
        }
    }

    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("ctrl".to_string());
        }
        if self.alt {
            parts.push("alt".to_string());
        }
        if self.shift {
            parts.push("shift".to_string());
        }
        if self.cmd {
            parts.push("cmd".to_string());
        }
        match &self.key {
            Key::Char(c) => parts.push(c.to_string()),
            Key::Named(s) => parts.push(s.clone()),
        }
        parts.join("-")
    }
}

impl CompiledKeymap {
    pub fn compile(config: &Config) -> Self {
        let mut fn_table: HashMap<String, Vec<Spanned<Statement>>> = HashMap::new();
        let mut bindings = Vec::new();

        for block in &config.blocks {
            if block.node.event != EventType::Key {
                continue;
            }

            // First pass: collect fn declarations
            for stmt in &block.node.statements {
                if let Statement::FnDecl(fn_decl) = &stmt.node {
                    fn_table.insert(fn_decl.name.node.clone(), fn_decl.body.clone());
                }
            }

            // Second pass: compile statements
            compile_statements(&block.node.statements, &[], &fn_table, &mut bindings);
        }

        Self { bindings }
    }

    pub fn lookup<'a>(
        &'a self,
        key: &CompiledKey,
        state: &dyn KeymapState,
    ) -> Option<&'a CompiledBinding> {
        self.bindings.iter().find(|binding| {
            binding.key == *key
                && binding
                    .predicates
                    .iter()
                    .all(|pred| evaluate_predicate(pred, state))
        })
    }

    pub fn reverse_lookup(
        &self,
        action: &str,
        state: &dyn KeymapState,
    ) -> Option<&CompiledBinding> {
        self.bindings.iter().find(|binding| {
            action_name(&binding.action) == action
                && binding
                    .predicates
                    .iter()
                    .all(|p| evaluate_predicate(p, state))
        })
    }
}

fn compile_statements(
    stmts: &[Spanned<Statement>],
    parent_predicates: &[Predicate],
    fn_table: &HashMap<String, Vec<Spanned<Statement>>>,
    out: &mut Vec<CompiledBinding>,
) {
    for stmt in stmts {
        match &stmt.node {
            Statement::Binding(binding) => {
                out.push(CompiledBinding {
                    key: CompiledKey::from_key_part(&binding.key.node),
                    predicates: parent_predicates.to_vec(),
                    action: strip_action_spans(&binding.action.node),
                });
            },
            Statement::PredicateBlock(block) => {
                let mut combined = parent_predicates.to_vec();
                combined.push(strip_predicate_spans(&block.predicate.node));
                compile_statements(&block.body, &combined, fn_table, out);
            },
            Statement::FnCall(name) => {
                if let Some(body) = fn_table.get(&name.node) {
                    compile_statements(body, parent_predicates, fn_table, out);
                }
            },
            Statement::FnDecl(_) => {},
            _ => {},
        }
    }
}

fn strip_predicate_spans(pred: &Predicate) -> Predicate {
    match pred {
        Predicate::Eq(field, val) => Predicate::Eq(field.clone(), val.clone()),
        Predicate::NotEq(field, val) => Predicate::NotEq(field.clone(), val.clone()),
        Predicate::Gt(field, val) => Predicate::Gt(field.clone(), val.clone()),
        Predicate::Lt(field, val) => Predicate::Lt(field.clone(), val.clone()),
        Predicate::Gte(field, val) => Predicate::Gte(field.clone(), val.clone()),
        Predicate::Lte(field, val) => Predicate::Lte(field.clone(), val.clone()),
        Predicate::Matches(field, pattern) => Predicate::Matches(field.clone(), pattern.clone()),
        Predicate::Bool(field) => Predicate::Bool(field.clone()),
        Predicate::And(l, r) => Predicate::And(
            Box::new(Spanned::new(strip_predicate_spans(&l.node), l.span.clone())),
            Box::new(Spanned::new(strip_predicate_spans(&r.node), r.span.clone())),
        ),
        Predicate::Or(l, r) => Predicate::Or(
            Box::new(Spanned::new(strip_predicate_spans(&l.node), l.span.clone())),
            Box::new(Spanned::new(strip_predicate_spans(&r.node), r.span.clone())),
        ),
    }
}

fn strip_action_spans(action_expr: &ActionExpr) -> ActionExpr {
    match action_expr {
        ActionExpr::Single(action) => ActionExpr::Single(Action {
            name: action.name.clone(),
            args: action.args.clone(),
        }),
        ActionExpr::Sequence(actions) => ActionExpr::Sequence(
            actions
                .iter()
                .map(|a| {
                    Spanned::new(
                        Action {
                            name: a.node.name.clone(),
                            args: a.node.args.clone(),
                        },
                        a.span.clone(),
                    )
                })
                .collect(),
        ),
    }
}

pub trait KeymapState {
    fn get_string(&self, name: &str) -> Option<&str>;
    fn get_number(&self, name: &str) -> Option<f64>;
    fn get_bool(&self, name: &str) -> Option<bool>;
}

pub fn evaluate_predicate(predicate: &Predicate, state: &dyn KeymapState) -> bool {
    match predicate {
        Predicate::And(l, r) => {
            evaluate_predicate(&l.node, state) && evaluate_predicate(&r.node, state)
        },
        Predicate::Or(l, r) => {
            evaluate_predicate(&l.node, state) || evaluate_predicate(&r.node, state)
        },
        Predicate::Eq(field, val) => match &val.node {
            Value::String(s) => state.get_string(&field.node).is_some_and(|v| v == s),
            Value::Ident(s) => state.get_string(&field.node).is_some_and(|v| v == s),
            Value::Number(n) => state.get_number(&field.node).is_some_and(|v| v == *n),
            Value::Bool(b) => state.get_bool(&field.node).is_some_and(|v| v == *b),
            _ => false,
        },
        Predicate::NotEq(field, val) => match &val.node {
            Value::String(s) => state.get_string(&field.node).is_some_and(|v| v != s),
            Value::Ident(s) => state.get_string(&field.node).is_some_and(|v| v != s),
            Value::Number(n) => state.get_number(&field.node).is_some_and(|v| v != *n),
            Value::Bool(b) => state.get_bool(&field.node).is_some_and(|v| v != *b),
            _ => false,
        },
        Predicate::Gt(field, val) => {
            if let Value::Number(n) = &val.node {
                state.get_number(&field.node).is_some_and(|v| v > *n)
            } else {
                false
            }
        },
        Predicate::Lt(field, val) => {
            if let Value::Number(n) = &val.node {
                state.get_number(&field.node).is_some_and(|v| v < *n)
            } else {
                false
            }
        },
        Predicate::Gte(field, val) => {
            if let Value::Number(n) = &val.node {
                state.get_number(&field.node).is_some_and(|v| v >= *n)
            } else {
                false
            }
        },
        Predicate::Lte(field, val) => {
            if let Value::Number(n) = &val.node {
                state.get_number(&field.node).is_some_and(|v| v <= *n)
            } else {
                false
            }
        },
        Predicate::Matches(field, pattern) => {
            if let Some(val) = state.get_string(&field.node) {
                glob::Pattern::new(&pattern.node)
                    .map(|p| p.matches(val))
                    .unwrap_or(false)
            } else {
                false
            }
        },
        Predicate::Bool(field) => state.get_bool(&field.node).unwrap_or(false),
    }
}

pub fn action_first_string_arg(action_expr: &ActionExpr) -> Option<String> {
    let action = match action_expr {
        ActionExpr::Single(a) => a,
        ActionExpr::Sequence(seq) => seq.first().map(|s| &s.node)?,
    };
    action.args.first().and_then(|arg| match &arg.node {
        Arg::Positional(val) => match &val.node {
            Value::String(s) => Some(s.clone()),
            Value::Ident(s) => Some(s.clone()),
            _ => None,
        },
        Arg::Named { value, .. } => match &value.node {
            Value::String(s) => Some(s.clone()),
            Value::Ident(s) => Some(s.clone()),
            _ => None,
        },
    })
}

pub fn action_name(action_expr: &ActionExpr) -> &str {
    match action_expr {
        ActionExpr::Single(a) => &a.name,
        ActionExpr::Sequence(seq) => seq.first().map(|s| s.node.name.as_str()).unwrap_or(""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestState {
        strings: HashMap<String, String>,
        numbers: HashMap<String, f64>,
        bools: HashMap<String, bool>,
    }

    impl TestState {
        fn new() -> Self {
            Self {
                strings: HashMap::new(),
                numbers: HashMap::new(),
                bools: HashMap::new(),
            }
        }

        fn with_string(mut self, k: &str, v: &str) -> Self {
            self.strings.insert(k.into(), v.into());
            self
        }

        fn with_number(mut self, k: &str, v: f64) -> Self {
            self.numbers.insert(k.into(), v);
            self
        }

        fn with_bool(mut self, k: &str, v: bool) -> Self {
            self.bools.insert(k.into(), v);
            self
        }
    }

    impl KeymapState for TestState {
        fn get_string(&self, name: &str) -> Option<&str> {
            self.strings.get(name).map(|s| s.as_str())
        }

        fn get_number(&self, name: &str) -> Option<f64> {
            self.numbers.get(name).copied()
        }

        fn get_bool(&self, name: &str) -> Option<bool> {
            self.bools.get(name).copied()
        }
    }

    fn parse_ok(source: &str) -> Config {
        let (result, errors) = stoat_config::parse(source);
        if !errors.is_empty() {
            panic!(
                "parse errors:\n{}",
                stoat_config::format_errors(source, &errors)
            );
        }
        result.expect("expected successful parse")
    }

    fn compile(source: &str) -> CompiledKeymap {
        let config = parse_ok(source);
        CompiledKeymap::compile(&config)
    }

    #[test]
    fn from_key_part_simple_char() {
        let kp = KeyPart {
            keys: vec![Key::Char('h')],
        };
        let ck = CompiledKey::from_key_part(&kp);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: false,
                alt: false,
                shift: false,
                cmd: false,
                key: Key::Char('h'),
            }
        );
    }

    #[test]
    fn from_key_part_uppercase_char() {
        let kp = KeyPart {
            keys: vec![Key::Char('G')],
        };
        let ck = CompiledKey::from_key_part(&kp);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: false,
                alt: false,
                shift: false,
                cmd: false,
                key: Key::Char('G'),
            }
        );
    }

    #[test]
    fn from_key_part_ctrl_s_shorthand() {
        let kp = KeyPart {
            keys: vec![Key::Char('C'), Key::Char('s')],
        };
        let ck = CompiledKey::from_key_part(&kp);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: true,
                alt: false,
                shift: false,
                cmd: false,
                key: Key::Char('s'),
            }
        );
    }

    #[test]
    fn from_key_part_ctrl_long_name() {
        let kp = KeyPart {
            keys: vec![Key::Named("Ctrl".into()), Key::Char('s')],
        };
        let ck = CompiledKey::from_key_part(&kp);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: true,
                alt: false,
                shift: false,
                cmd: false,
                key: Key::Char('s'),
            }
        );
    }

    #[test]
    fn from_key_part_shift_x_normalizes() {
        let kp = KeyPart {
            keys: vec![Key::Char('S'), Key::Char('x')],
        };
        let ck = CompiledKey::from_key_part(&kp);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: false,
                alt: false,
                shift: false,
                cmd: false,
                key: Key::Char('X'),
            }
        );
    }

    #[test]
    fn from_key_part_ctrl_shift_l() {
        let kp = KeyPart {
            keys: vec![Key::Char('C'), Key::Char('S'), Key::Char('l')],
        };
        let ck = CompiledKey::from_key_part(&kp);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: true,
                alt: false,
                shift: false,
                cmd: false,
                key: Key::Char('L'),
            }
        );
    }

    #[test]
    fn from_key_part_ctrl_uppercase_l() {
        let kp = KeyPart {
            keys: vec![Key::Named("ctrl".into()), Key::Char('L')],
        };
        let ck = CompiledKey::from_key_part(&kp);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: true,
                alt: false,
                shift: false,
                cmd: false,
                key: Key::Char('L'),
            }
        );
    }

    #[test]
    fn from_key_part_shift_tab() {
        let kp = KeyPart {
            keys: vec![Key::Char('S'), Key::Named("Tab".into())],
        };
        let ck = CompiledKey::from_key_part(&kp);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: false,
                alt: false,
                shift: true,
                cmd: false,
                key: Key::Named("tab".into()),
            }
        );
    }

    #[test]
    fn from_key_part_escape() {
        let kp = KeyPart {
            keys: vec![Key::Named("Escape".into())],
        };
        let ck = CompiledKey::from_key_part(&kp);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: false,
                alt: false,
                shift: false,
                cmd: false,
                key: Key::Named("escape".into()),
            }
        );
    }

    #[test]
    fn from_key_part_cmd() {
        let kp = KeyPart {
            keys: vec![Key::Named("Cmd".into()), Key::Char('s')],
        };
        let ck = CompiledKey::from_key_part(&kp);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: false,
                alt: false,
                shift: false,
                cmd: true,
                key: Key::Char('s'),
            }
        );
    }

    #[test]
    fn from_key_part_alt() {
        let kp = KeyPart {
            keys: vec![Key::Char('M'), Key::Char('x')],
        };
        let ck = CompiledKey::from_key_part(&kp);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: false,
                alt: true,
                shift: false,
                cmd: false,
                key: Key::Char('x'),
            }
        );
    }

    #[test]
    fn from_keystroke_simple() {
        let ks = gpui::Keystroke {
            key: "h".into(),
            modifiers: gpui::Modifiers::default(),
            key_char: Some("h".into()),
        };
        let ck = CompiledKey::from_keystroke(&ks);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: false,
                alt: false,
                shift: false,
                cmd: false,
                key: Key::Char('h'),
            }
        );
    }

    #[test]
    fn from_keystroke_ctrl_s() {
        let ks = gpui::Keystroke {
            key: "s".into(),
            modifiers: gpui::Modifiers {
                control: true,
                ..Default::default()
            },
            key_char: None,
        };
        let ck = CompiledKey::from_keystroke(&ks);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: true,
                alt: false,
                shift: false,
                cmd: false,
                key: Key::Char('s'),
            }
        );
    }

    #[test]
    fn from_keystroke_escape() {
        let ks = gpui::Keystroke {
            key: "escape".into(),
            modifiers: gpui::Modifiers::default(),
            key_char: None,
        };
        let ck = CompiledKey::from_keystroke(&ks);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: false,
                alt: false,
                shift: false,
                cmd: false,
                key: Key::Named("escape".into()),
            }
        );
    }

    #[test]
    fn from_keystroke_shift_tab() {
        let ks = gpui::Keystroke {
            key: "tab".into(),
            modifiers: gpui::Modifiers {
                shift: true,
                ..Default::default()
            },
            key_char: None,
        };
        let ck = CompiledKey::from_keystroke(&ks);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: false,
                alt: false,
                shift: true,
                cmd: false,
                key: Key::Named("tab".into()),
            }
        );
    }

    #[test]
    fn from_keystroke_ctrl_shift_l() {
        let ks = gpui::Keystroke {
            key: "l".into(),
            modifiers: gpui::Modifiers {
                control: true,
                shift: true,
                ..Default::default()
            },
            key_char: None,
        };
        let ck = CompiledKey::from_keystroke(&ks);
        assert_eq!(
            ck,
            CompiledKey {
                ctrl: true,
                alt: false,
                shift: false,
                cmd: false,
                key: Key::Char('L'),
            }
        );
    }

    #[test]
    fn cross_verify_ctrl_s() {
        let kp = KeyPart {
            keys: vec![Key::Char('C'), Key::Char('s')],
        };
        let ks = gpui::Keystroke {
            key: "s".into(),
            modifiers: gpui::Modifiers {
                control: true,
                ..Default::default()
            },
            key_char: None,
        };
        assert_eq!(
            CompiledKey::from_key_part(&kp),
            CompiledKey::from_keystroke(&ks)
        );
    }

    #[test]
    fn cross_verify_ctrl_shift_l() {
        let kp = KeyPart {
            keys: vec![Key::Char('C'), Key::Char('S'), Key::Char('l')],
        };
        let ks = gpui::Keystroke {
            key: "l".into(),
            modifiers: gpui::Modifiers {
                control: true,
                shift: true,
                ..Default::default()
            },
            key_char: None,
        };
        assert_eq!(
            CompiledKey::from_key_part(&kp),
            CompiledKey::from_keystroke(&ks)
        );
    }

    #[test]
    fn cross_verify_escape() {
        let kp = KeyPart {
            keys: vec![Key::Named("Escape".into())],
        };
        let ks = gpui::Keystroke {
            key: "escape".into(),
            modifiers: gpui::Modifiers::default(),
            key_char: None,
        };
        assert_eq!(
            CompiledKey::from_key_part(&kp),
            CompiledKey::from_keystroke(&ks)
        );
    }

    #[test]
    fn cross_verify_shift_tab() {
        let kp = KeyPart {
            keys: vec![Key::Char('S'), Key::Named("Tab".into())],
        };
        let ks = gpui::Keystroke {
            key: "tab".into(),
            modifiers: gpui::Modifiers {
                shift: true,
                ..Default::default()
            },
            key_char: None,
        };
        assert_eq!(
            CompiledKey::from_key_part(&kp),
            CompiledKey::from_keystroke(&ks)
        );
    }

    #[test]
    fn eval_eq_string() {
        let pred = Predicate::Eq(
            Spanned::new("mode".into(), 0..0),
            Spanned::new(Value::String("normal".into()), 0..0),
        );
        let state = TestState::new().with_string("mode", "normal");
        assert!(evaluate_predicate(&pred, &state));
    }

    #[test]
    fn eval_eq_string_mismatch() {
        let pred = Predicate::Eq(
            Spanned::new("mode".into(), 0..0),
            Spanned::new(Value::String("normal".into()), 0..0),
        );
        let state = TestState::new().with_string("mode", "insert");
        assert!(!evaluate_predicate(&pred, &state));
    }

    #[test]
    fn eval_not_eq() {
        let pred = Predicate::NotEq(
            Spanned::new("mode".into(), 0..0),
            Spanned::new(Value::String("insert".into()), 0..0),
        );
        let state = TestState::new().with_string("mode", "normal");
        assert!(evaluate_predicate(&pred, &state));
    }

    #[test]
    fn eval_gt() {
        let pred = Predicate::Gt(
            Spanned::new("cursor_line".into(), 0..0),
            Spanned::new(Value::Number(5.0), 0..0),
        );
        let state = TestState::new().with_number("cursor_line", 10.0);
        assert!(evaluate_predicate(&pred, &state));
        let state2 = TestState::new().with_number("cursor_line", 3.0);
        assert!(!evaluate_predicate(&pred, &state2));
    }

    #[test]
    fn eval_lt() {
        let pred = Predicate::Lt(
            Spanned::new("cursor_line".into(), 0..0),
            Spanned::new(Value::Number(5.0), 0..0),
        );
        let state = TestState::new().with_number("cursor_line", 3.0);
        assert!(evaluate_predicate(&pred, &state));
    }

    #[test]
    fn eval_gte() {
        let pred = Predicate::Gte(
            Spanned::new("cursor_line".into(), 0..0),
            Spanned::new(Value::Number(5.0), 0..0),
        );
        let state = TestState::new().with_number("cursor_line", 5.0);
        assert!(evaluate_predicate(&pred, &state));
        let state2 = TestState::new().with_number("cursor_line", 6.0);
        assert!(evaluate_predicate(&pred, &state2));
    }

    #[test]
    fn eval_lte() {
        let pred = Predicate::Lte(
            Spanned::new("cursor_line".into(), 0..0),
            Spanned::new(Value::Number(5.0), 0..0),
        );
        let state = TestState::new().with_number("cursor_line", 5.0);
        assert!(evaluate_predicate(&pred, &state));
        let state2 = TestState::new().with_number("cursor_line", 4.0);
        assert!(evaluate_predicate(&pred, &state2));
    }

    #[test]
    fn eval_matches() {
        let pred = Predicate::Matches(
            Spanned::new("path".into(), 0..0),
            Spanned::new("*.rs".into(), 0..0),
        );
        let state = TestState::new().with_string("path", "main.rs");
        assert!(evaluate_predicate(&pred, &state));
        let state2 = TestState::new().with_string("path", "main.py");
        assert!(!evaluate_predicate(&pred, &state2));
    }

    #[test]
    fn eval_bool() {
        let pred = Predicate::Bool(Spanned::new("has_selection".into(), 0..0));
        let state = TestState::new().with_bool("has_selection", true);
        assert!(evaluate_predicate(&pred, &state));
        let state2 = TestState::new().with_bool("has_selection", false);
        assert!(!evaluate_predicate(&pred, &state2));
        let state3 = TestState::new();
        assert!(!evaluate_predicate(&pred, &state3));
    }

    #[test]
    fn eval_and() {
        let pred = Predicate::And(
            Box::new(Spanned::new(
                Predicate::Eq(
                    Spanned::new("focus".into(), 0..0),
                    Spanned::new(Value::String("TextEditor".into()), 0..0),
                ),
                0..0,
            )),
            Box::new(Spanned::new(
                Predicate::Eq(
                    Spanned::new("mode".into(), 0..0),
                    Spanned::new(Value::String("normal".into()), 0..0),
                ),
                0..0,
            )),
        );
        let state = TestState::new()
            .with_string("focus", "TextEditor")
            .with_string("mode", "normal");
        assert!(evaluate_predicate(&pred, &state));
        let state2 = TestState::new()
            .with_string("focus", "TextEditor")
            .with_string("mode", "insert");
        assert!(!evaluate_predicate(&pred, &state2));
    }

    #[test]
    fn eval_or() {
        let pred = Predicate::Or(
            Box::new(Spanned::new(
                Predicate::Eq(
                    Spanned::new("mode".into(), 0..0),
                    Spanned::new(Value::String("normal".into()), 0..0),
                ),
                0..0,
            )),
            Box::new(Spanned::new(
                Predicate::Eq(
                    Spanned::new("mode".into(), 0..0),
                    Spanned::new(Value::String("visual".into()), 0..0),
                ),
                0..0,
            )),
        );
        let state = TestState::new().with_string("mode", "visual");
        assert!(evaluate_predicate(&pred, &state));
        let state2 = TestState::new().with_string("mode", "insert");
        assert!(!evaluate_predicate(&pred, &state2));
    }

    #[test]
    fn compile_simple_binding() {
        let km = compile("on key { h -> MoveLeft(); }");
        assert_eq!(km.bindings.len(), 1);
        assert_eq!(
            km.bindings[0].key,
            CompiledKey {
                ctrl: false,
                alt: false,
                shift: false,
                cmd: false,
                key: Key::Char('h'),
            }
        );
        assert!(km.bindings[0].predicates.is_empty());
    }

    #[test]
    fn compile_nested_predicates_flattened() {
        let km = compile(
            r#"on key {
                focus == "TextEditor" {
                    mode == "normal" {
                        j -> MoveDown();
                    }
                }
            }"#,
        );
        assert_eq!(km.bindings.len(), 1);
        assert_eq!(km.bindings[0].predicates.len(), 2);
    }

    #[test]
    fn compile_fn_inlining() {
        let km = compile(
            r#"on key {
                fn motions() {
                    h -> MoveLeft();
                    l -> MoveRight();
                }
                focus == "TextEditor" {
                    motions();
                }
            }"#,
        );
        assert_eq!(km.bindings.len(), 2);
        assert_eq!(km.bindings[0].predicates.len(), 1);
        assert_eq!(km.bindings[1].predicates.len(), 1);
    }

    #[test]
    fn lookup_with_matching_state() {
        let km = compile(
            r#"on key {
                focus == "TextEditor" {
                    mode == "normal" {
                        h -> MoveLeft();
                    }
                }
            }"#,
        );
        let state = TestState::new()
            .with_string("focus", "TextEditor")
            .with_string("mode", "normal");
        let key = CompiledKey {
            ctrl: false,
            alt: false,
            shift: false,
            cmd: false,
            key: Key::Char('h'),
        };
        assert!(km.lookup(&key, &state).is_some());
    }

    #[test]
    fn lookup_with_non_matching_state() {
        let km = compile(
            r#"on key {
                focus == "TextEditor" {
                    mode == "normal" {
                        h -> MoveLeft();
                    }
                }
            }"#,
        );
        let state = TestState::new()
            .with_string("focus", "TextEditor")
            .with_string("mode", "insert");
        let key = CompiledKey {
            ctrl: false,
            alt: false,
            shift: false,
            cmd: false,
            key: Key::Char('h'),
        };
        assert!(km.lookup(&key, &state).is_none());
    }

    #[test]
    fn lookup_first_match_wins() {
        let km = compile(
            r#"on key {
                focus == "TextEditor" {
                    h -> MoveLeft();
                    h -> MoveRight();
                }
            }"#,
        );
        let state = TestState::new().with_string("focus", "TextEditor");
        let key = CompiledKey {
            ctrl: false,
            alt: false,
            shift: false,
            cmd: false,
            key: Key::Char('h'),
        };
        let binding = km.lookup(&key, &state).unwrap();
        match &binding.action {
            ActionExpr::Single(a) => assert_eq!(a.name, "MoveLeft"),
            _ => panic!("expected single action"),
        }
    }

    #[test]
    fn compile_full_keymap_stcfg() {
        let source = include_str!("../../../keymap.stcfg");
        let config = parse_ok(source);
        let km = CompiledKeymap::compile(&config);
        assert!(!km.bindings.is_empty());

        let state = TestState::new()
            .with_string("focus", "TextEditor")
            .with_string("mode", "normal");

        let h_key = CompiledKey {
            ctrl: false,
            alt: false,
            shift: false,
            cmd: false,
            key: Key::Char('h'),
        };
        let binding = km.lookup(&h_key, &state).unwrap();
        assert_eq!(action_name(&binding.action), "MoveLeft");

        let esc_key = CompiledKey {
            ctrl: false,
            alt: false,
            shift: false,
            cmd: false,
            key: Key::Named("escape".into()),
        };
        let state_insert = TestState::new()
            .with_string("focus", "TextEditor")
            .with_string("mode", "insert");
        let binding = km.lookup(&esc_key, &state_insert).unwrap();
        assert_eq!(action_name(&binding.action), "SetMode");
    }
}
