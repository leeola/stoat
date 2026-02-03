mod ast;
mod error;
mod parser;

pub use ast::{
    ActionCall, ActionExpr, Arg, Binding, Config, EventBlock, EventType, Expr, FnDecl, KeyCombo,
    KeyPart, LetBinding, Predicate, PredicateBlock, PrefixBlock, Setting, Span, Spanned, Statement,
    Value,
};
pub use error::{format_errors, ParseError};

pub fn parse(source: &str) -> (Option<Config>, Vec<ParseError>) {
    parser::parse(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(source: &str) -> Config {
        let (result, errors) = parse(source);
        if !errors.is_empty() {
            panic!("parse errors:\n{}", format_errors(source, &errors));
        }
        result.expect("expected successful parse")
    }

    fn assert_binding(stmt: &Spanned<Statement>) -> &Binding {
        match &stmt.node {
            Statement::Binding(b) => b,
            _ => panic!("expected binding, got {:?}", stmt),
        }
    }

    fn assert_predicate_block(stmt: &Spanned<Statement>) -> &PredicateBlock {
        match &stmt.node {
            Statement::PredicateBlock(s) => s,
            _ => panic!("expected predicate block, got {:?}", stmt),
        }
    }

    fn assert_setting(stmt: &Spanned<Statement>) -> &Setting {
        match &stmt.node {
            Statement::Setting(s) => s,
            _ => panic!("expected setting, got {:?}", stmt),
        }
    }

    fn assert_let(stmt: &Spanned<Statement>) -> &LetBinding {
        match &stmt.node {
            Statement::Let(l) => l,
            _ => panic!("expected let binding, got {:?}", stmt),
        }
    }

    fn assert_fn_decl(stmt: &Spanned<Statement>) -> &FnDecl {
        match &stmt.node {
            Statement::FnDecl(f) => f,
            _ => panic!("expected fn decl, got {:?}", stmt),
        }
    }

    fn assert_prefix_block(stmt: &Spanned<Statement>) -> &PrefixBlock {
        match &stmt.node {
            Statement::PrefixBlock(p) => p,
            _ => panic!("expected prefix block, got {:?}", stmt),
        }
    }

    #[test]
    fn empty_config() {
        let config = parse_ok("");
        assert!(config.blocks.is_empty());
    }

    #[test]
    fn event_blocks() {
        let config = parse_ok(
            r#"
            on init {
            }
            on buffer {
            }
            on key {
            }
            "#,
        );
        assert_eq!(config.blocks.len(), 3);
        assert_eq!(config.blocks[0].node.event, EventType::Init);
        assert_eq!(config.blocks[1].node.event, EventType::Buffer);
        assert_eq!(config.blocks[2].node.event, EventType::Key);
    }

    #[test]
    fn basic_binding() {
        let config = parse_ok(
            r#"
            on key {
                ctrl+s -> Save()
            }
            "#,
        );
        let block = &config.blocks[0].node;
        assert_eq!(block.event, EventType::Key);
        assert_eq!(block.statements.len(), 1);
        let binding = assert_binding(&block.statements[0]);
        assert_eq!(binding.key.node.keys.len(), 1);
        assert_eq!(binding.key.node.keys[0].modifiers, vec!["ctrl"]);
        assert_eq!(binding.key.node.keys[0].key, "s");
        match &binding.action.node {
            ActionExpr::Single(call) => {
                assert_eq!(call.name, "Save");
                assert!(call.args.is_empty());
            },
            _ => panic!("expected single action"),
        }
    }

    #[test]
    fn key_sequences() {
        let config = parse_ok("on key { g g -> GoToLine(first) }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(binding.key.node.keys.len(), 2);
        assert_eq!(binding.key.node.keys[0].key, "g");
        assert_eq!(binding.key.node.keys[1].key, "g");
    }

    #[test]
    fn multiple_modifiers() {
        let config = parse_ok("on key { ctrl+shift+alt+k -> Kill() }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(
            binding.key.node.keys[0].modifiers,
            vec!["ctrl", "shift", "alt"]
        );
        assert_eq!(binding.key.node.keys[0].key, "k");
    }

    #[test]
    fn settings() {
        let config = parse_ok(
            r#"
            on init {
                font.size = 14
                editor.tab_size = 4
            }
            "#,
        );
        let block = &config.blocks[0].node;
        assert_eq!(block.statements.len(), 2);

        let setting1 = assert_setting(&block.statements[0]);
        assert_eq!(setting1.path.len(), 2);
        assert_eq!(setting1.path[0].node, "font");
        assert_eq!(setting1.path[1].node, "size");
        assert_eq!(setting1.value.node, Value::Int(14));

        let setting2 = assert_setting(&block.statements[1]);
        assert_eq!(setting2.path.len(), 2);
        assert_eq!(setting2.path[0].node, "editor");
        assert_eq!(setting2.path[1].node, "tab_size");
        assert_eq!(setting2.value.node, Value::Int(4));
    }

    #[test]
    fn float_values() {
        let config = parse_ok("on init { font.size = 14.5 }");
        let setting = assert_setting(&config.blocks[0].node.statements[0]);
        assert_eq!(setting.value.node, Value::Float(14.5));
    }

    #[test]
    fn string_values_quoted() {
        let config = parse_ok(r#"on key { mode == "normal" { j -> MoveDown() } }"#);
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Eq(field, val) => {
                assert_eq!(field.node, "mode");
                assert_eq!(val.node, Value::String("normal".to_string()));
            },
            _ => panic!("expected Eq predicate"),
        }
    }

    #[test]
    fn ident_values_unquoted() {
        let config = parse_ok("on key { mode == normal { j -> MoveDown() } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Eq(field, val) => {
                assert_eq!(field.node, "mode");
                assert_eq!(val.node, Value::Ident("normal".to_string()));
            },
            _ => panic!("expected Eq predicate"),
        }
    }

    #[test]
    fn enum_values() {
        let config = parse_ok("on init { platform == Platform::MacOS { cmd_as_meta = true } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Eq(field, val) => {
                assert_eq!(field.node, "platform");
                assert_eq!(
                    val.node,
                    Value::Enum("Platform".to_string(), "MacOS".to_string())
                );
            },
            _ => panic!("expected Eq predicate"),
        }
    }

    #[test]
    fn array_values() {
        let config = parse_ok("on init { editor.rulers = [79, 100] }");
        let setting = assert_setting(&config.blocks[0].node.statements[0]);
        assert_eq!(
            setting.value.node,
            Value::Array(vec![Value::Int(79), Value::Int(100)])
        );
    }

    #[test]
    fn predicate_equality() {
        let config = parse_ok(r#"on key { mode == "normal" { j -> MoveDown() } }"#);
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Eq(field, val) => {
                assert_eq!(field.node, "mode");
                assert_eq!(val.node, Value::String("normal".to_string()));
            },
            _ => panic!("expected Eq predicate"),
        }
    }

    #[test]
    fn focus_predicate() {
        let config = parse_ok(r#"on key { focus == "TextEditor" { ctrl+s -> Save() } }"#);
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Eq(field, val) => {
                assert_eq!(field.node, "focus");
                assert_eq!(val.node, Value::String("TextEditor".to_string()));
            },
            _ => panic!("expected Eq predicate"),
        }
    }

    #[test]
    fn predicate_comparisons() {
        let config = parse_ok("on key { cursor_line > 1 { k -> MoveUp() } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Gt(field, val) => {
                assert_eq!(field.node, "cursor_line");
                assert_eq!(val.node, Value::Int(1));
            },
            _ => panic!("expected Gt predicate"),
        }

        let config = parse_ok("on key { cursor_line < 10 { j -> MoveDown() } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Lt(field, val) => {
                assert_eq!(field.node, "cursor_line");
                assert_eq!(val.node, Value::Int(10));
            },
            _ => panic!("expected Lt predicate"),
        }

        let config = parse_ok("on key { cursor_line >= 1 { k -> MoveUp() } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Ge(field, val) => {
                assert_eq!(field.node, "cursor_line");
                assert_eq!(val.node, Value::Int(1));
            },
            _ => panic!("expected Ge predicate"),
        }

        let config = parse_ok("on key { cursor_line <= 10 { j -> MoveDown() } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Le(field, val) => {
                assert_eq!(field.node, "cursor_line");
                assert_eq!(val.node, Value::Int(10));
            },
            _ => panic!("expected Le predicate"),
        }
    }

    #[test]
    fn predicate_and() {
        let config = parse_ok(r#"on key { mode == "normal" && has_selection { d -> Delete() } }"#);
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::And(left, right) => {
                match &left.node {
                    Predicate::Eq(field, val) => {
                        assert_eq!(field.node, "mode");
                        assert_eq!(val.node, Value::String("normal".to_string()));
                    },
                    _ => panic!("expected Eq predicate on left"),
                }
                match &right.node {
                    Predicate::Var(name) => assert_eq!(name.node, "has_selection"),
                    _ => panic!("expected Var predicate on right"),
                }
            },
            _ => panic!("expected And predicate"),
        }
    }

    #[test]
    fn predicate_or() {
        let config = parse_ok(r#"on key { mode == "normal" || mode == "visual" { y -> Yank() } }"#);
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Or(left, right) => {
                match &left.node {
                    Predicate::Eq(field, val) => {
                        assert_eq!(field.node, "mode");
                        assert_eq!(val.node, Value::String("normal".to_string()));
                    },
                    _ => panic!("expected Eq predicate on left"),
                }
                match &right.node {
                    Predicate::Eq(field, val) => {
                        assert_eq!(field.node, "mode");
                        assert_eq!(val.node, Value::String("visual".to_string()));
                    },
                    _ => panic!("expected Eq predicate on right"),
                }
            },
            _ => panic!("expected Or predicate"),
        }
    }

    #[test]
    fn predicate_not() {
        let config = parse_ok("on key { !has_selection { i -> Insert() } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Not(inner) => match &inner.node {
                Predicate::Var(name) => assert_eq!(name.node, "has_selection"),
                _ => panic!("expected Var predicate"),
            },
            _ => panic!("expected Not predicate"),
        }
    }

    #[test]
    fn predicate_grouping() {
        let config = parse_ok(
            r#"on key { (mode == "normal" || mode == "visual") && cursor_line > 1 { k -> MoveUp() } }"#,
        );
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::And(left, right) => {
                match &left.node {
                    Predicate::Or(_, _) => {},
                    _ => panic!("expected Or predicate on left"),
                }
                match &right.node {
                    Predicate::Gt(field, val) => {
                        assert_eq!(field.node, "cursor_line");
                        assert_eq!(val.node, Value::Int(1));
                    },
                    _ => panic!("expected Gt predicate on right"),
                }
            },
            _ => panic!("expected And predicate"),
        }
    }

    #[test]
    fn predicate_matches() {
        let config = parse_ok(r#"on buffer { path ~ "*.rs" { rust_mode = true } }"#);
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Matches(field, pattern) => {
                assert_eq!(field.node, "path");
                assert_eq!(pattern.node, "*.rs");
            },
            _ => panic!("expected Matches predicate"),
        }
    }

    #[test]
    fn nested_predicate_blocks() {
        let config = parse_ok(
            r#"
            on key {
                focus == "TextEditor" {
                    mode == "normal" {
                        j -> MoveDown()
                    }
                }
            }
            "#,
        );
        let outer = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &outer.predicate.node {
            Predicate::Eq(field, val) => {
                assert_eq!(field.node, "focus");
                assert_eq!(val.node, Value::String("TextEditor".to_string()));
            },
            _ => panic!("expected Eq predicate"),
        }
        let inner = assert_predicate_block(&outer.body[0]);
        match &inner.predicate.node {
            Predicate::Eq(field, val) => {
                assert_eq!(field.node, "mode");
                assert_eq!(val.node, Value::String("normal".to_string()));
            },
            _ => panic!("expected Eq predicate"),
        }
    }

    #[test]
    fn action_with_positional_args() {
        let config = parse_ok("on key { h -> MoveCursor(left) }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        match &binding.action.node {
            ActionExpr::Single(call) => {
                assert_eq!(call.name, "MoveCursor");
                assert_eq!(call.args.len(), 1);
                match &call.args[0] {
                    Arg::Positional(Expr::Ident(s)) => assert_eq!(s, "left"),
                    _ => panic!("expected positional ident arg"),
                }
            },
            _ => panic!("expected single action"),
        }
    }

    #[test]
    fn action_with_named_args() {
        let config = parse_ok("on key { h -> MoveCursor(direction: left, count: 1) }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        match &binding.action.node {
            ActionExpr::Single(call) => {
                assert_eq!(call.name, "MoveCursor");
                assert_eq!(call.args.len(), 2);
                match &call.args[0] {
                    Arg::Named(name, Expr::Ident(val)) => {
                        assert_eq!(name, "direction");
                        assert_eq!(val, "left");
                    },
                    _ => panic!("expected named ident arg"),
                }
                match &call.args[1] {
                    Arg::Named(name, Expr::Int(val)) => {
                        assert_eq!(name, "count");
                        assert_eq!(*val, 1);
                    },
                    _ => panic!("expected named int arg"),
                }
            },
            _ => panic!("expected single action"),
        }
    }

    #[test]
    fn state_refs() {
        let config = parse_ok("on key { j -> MoveDown($count) }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        match &binding.action.node {
            ActionExpr::Single(call) => match &call.args[0] {
                Arg::Positional(Expr::StateRef(name)) => assert_eq!(name, "count"),
                _ => panic!("expected state ref"),
            },
            _ => panic!("expected single action"),
        }
    }

    #[test]
    fn state_ref_with_default() {
        let config = parse_ok("on key { j -> MoveDown($count ?? 1) }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        match &binding.action.node {
            ActionExpr::Single(call) => match &call.args[0] {
                Arg::Positional(Expr::Default(left, right)) => {
                    match &**left {
                        Expr::StateRef(name) => assert_eq!(name, "count"),
                        _ => panic!("expected state ref"),
                    }
                    match &**right {
                        Expr::Int(n) => assert_eq!(*n, 1),
                        _ => panic!("expected int"),
                    }
                },
                _ => panic!("expected default expr"),
            },
            _ => panic!("expected single action"),
        }
    }

    #[test]
    fn action_sequences() {
        let config = parse_ok("on key { ctrl+k ctrl+c -> [SelectLine(), Comment()] }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        match &binding.action.node {
            ActionExpr::Sequence(actions) => {
                assert_eq!(actions.len(), 2);
                assert_eq!(actions[0].name, "SelectLine");
                assert_eq!(actions[1].name, "Comment");
            },
            _ => panic!("expected sequence"),
        }
    }

    #[test]
    fn let_bindings() {
        let config = parse_ok("on init { let leader = space }");
        let let_binding = assert_let(&config.blocks[0].node.statements[0]);
        assert_eq!(let_binding.name.node, "leader");
        match &let_binding.value.node {
            Expr::Ident(val) => assert_eq!(val, "space"),
            _ => panic!("expected ident"),
        }
    }

    #[test]
    fn if_expressions() {
        let config = parse_ok(r#"on init { let mod = if platform == "macos" then cmd else ctrl }"#);
        let let_binding = assert_let(&config.blocks[0].node.statements[0]);
        assert_eq!(let_binding.name.node, "mod");
        match &let_binding.value.node {
            Expr::If(cond, then_branch, else_branch) => {
                match &**cond {
                    Expr::Eq(left, right) => {
                        match &**left {
                            Expr::Ident(s) => assert_eq!(s, "platform"),
                            _ => panic!("expected ident on left side of =="),
                        }
                        match &**right {
                            Expr::String(s) => assert_eq!(s, "macos"),
                            _ => panic!("expected string on right side of =="),
                        }
                    },
                    _ => panic!("expected Eq in condition"),
                }
                match &**then_branch {
                    Expr::Ident(s) => assert_eq!(s, "cmd"),
                    _ => panic!("expected ident in then branch"),
                }
                match &**else_branch {
                    Expr::Ident(s) => assert_eq!(s, "ctrl"),
                    _ => panic!("expected ident in else branch"),
                }
            },
            _ => panic!("expected if expression"),
        }
    }

    #[test]
    fn fn_declarations() {
        let config = parse_ok(
            r#"
            on key {
                fn vim_motions() {
                    h -> MoveCursor(left)
                    j -> MoveCursor(down)
                    k -> MoveCursor(up)
                    l -> MoveCursor(right)
                }
            }
            "#,
        );
        let fn_decl = assert_fn_decl(&config.blocks[0].node.statements[0]);
        assert_eq!(fn_decl.name.node, "vim_motions");
        assert_eq!(fn_decl.body.len(), 4);
    }

    #[test]
    fn prefix_blocks() {
        let config = parse_ok(
            r#"
            on key {
                g {
                    g -> GoToLine(first)
                    e -> GoToLine(last)
                }
            }
            "#,
        );
        let prefix = assert_prefix_block(&config.blocks[0].node.statements[0]);
        assert_eq!(prefix.key.node.keys.len(), 1);
        assert_eq!(prefix.key.node.keys[0].key, "g");
        assert_eq!(prefix.body.len(), 2);
    }

    #[test]
    fn comments() {
        let config = parse_ok(
            r#"
            # This is a comment
            on key {
                ctrl+s -> Save()  # Inline comment
                # Another comment
            }
            "#,
        );
        assert_eq!(config.blocks.len(), 1);
        assert_eq!(config.blocks[0].node.statements.len(), 1);
    }

    #[test]
    fn error_recovery() {
        let source = r#"
            on key {
                ctrl+s -> Save()
                invalid syntax here @@@
                ctrl+q -> Quit()
            }
        "#;
        let (result, errors) = parse(source);
        assert!(!errors.is_empty(), "expected parse errors");
        let _ = result;
    }

    #[test]
    fn negative_integers() {
        let config = parse_ok("on key { offset > -10 { k -> MoveUp() } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Gt(field, val) => {
                assert_eq!(field.node, "offset");
                assert_eq!(val.node, Value::Int(-10));
            },
            _ => panic!("expected Gt predicate"),
        }
    }

    #[test]
    fn format_errors_output() {
        let source = "on key { ctrl+s -> @@@ }";
        let (_, errors) = parse(source);
        assert!(!errors.is_empty());
        let output = format_errors(source, &errors);
        assert!(!output.is_empty());
    }

    #[test]
    fn multiple_event_blocks() {
        let config = parse_ok(
            r#"
            on init {
                font.size = 14
                let leader = space
            }
            on buffer {
                path ~ "*.rs" {
                    rust_mode = true
                }
            }
            on key {
                ctrl+s -> Save()
                focus == "TextEditor" {
                    j -> MoveDown()
                    k -> MoveUp()
                }
            }
            "#,
        );
        assert_eq!(config.blocks.len(), 3);

        let init_block = &config.blocks[0].node;
        assert_eq!(init_block.event, EventType::Init);
        assert_eq!(init_block.statements.len(), 2);

        let buffer_block = &config.blocks[1].node;
        assert_eq!(buffer_block.event, EventType::Buffer);
        assert_eq!(buffer_block.statements.len(), 1);

        let key_block = &config.blocks[2].node;
        assert_eq!(key_block.event, EventType::Key);
        assert_eq!(key_block.statements.len(), 2);
    }

    #[test]
    fn bool_values() {
        let config = parse_ok("on init { read_only == true { ctrl+s -> Noop() } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Eq(field, val) => {
                assert_eq!(field.node, "read_only");
                assert_eq!(val.node, Value::Bool(true));
            },
            _ => panic!("expected Eq predicate"),
        }
    }

    #[test]
    fn predicate_inequality() {
        let config = parse_ok(r#"on key { mode != "insert" { j -> MoveDown() } }"#);
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Ne(field, val) => {
                assert_eq!(field.node, "mode");
                assert_eq!(val.node, Value::String("insert".to_string()));
            },
            _ => panic!("expected Ne predicate"),
        }
    }

    #[test]
    fn span_accuracy() {
        let source = "on init { font.size = 14 }";
        let config = parse_ok(source);
        let block_span = &config.blocks[0].span;
        assert_eq!(block_span.start, 0);
        assert_eq!(&source[block_span.clone()], source);

        let stmt_span = &config.blocks[0].node.statements[0].span;
        assert_eq!(&source[stmt_span.clone()], "font.size = 14");
    }

    #[test]
    fn state_ref_in_value() {
        let config = parse_ok("on init { count = $register_count }");
        let setting = assert_setting(&config.blocks[0].node.statements[0]);
        assert_eq!(
            setting.value.node,
            Value::StateRef("register_count".to_string())
        );
    }
}
