mod ast;
mod error;
mod parser;

pub use ast::{
    Action, ActionExpr, Arg, Binding, Config, EventBlock, EventType, Expr, FnDecl, Key, KeyPart,
    LetBinding, Predicate, PredicateBlock, Setting, Span, Spanned, Statement, Value,
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

    fn key_char(c: char) -> Key {
        Key::Char(c)
    }

    fn key_named(s: &str) -> Key {
        Key::Named(s.to_string())
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
    fn single_char_keys() {
        let config = parse_ok("on key { h -> MoveLeft(); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(binding.key.node.keys, vec![key_char('h')]);
    }

    #[test]
    fn uppercase_char_keys() {
        let config = parse_ok("on key { G -> MoveToFileEnd(); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(binding.key.node.keys, vec![key_char('G')]);
        match &binding.action.node {
            ActionExpr::Single(action) => assert_eq!(action.name, "MoveToFileEnd"),
            _ => panic!("expected single action"),
        }
    }

    #[test]
    fn digit_keys() {
        let config = parse_ok("on key { 0 -> MoveToLineStart(); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(binding.key.node.keys, vec![key_char('0')]);
    }

    #[test]
    fn punctuation_keys() {
        let config = parse_ok("on key { $ -> MoveToLineEnd(); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(binding.key.node.keys, vec![key_char('$')]);
    }

    #[test]
    fn colon_key() {
        let config = parse_ok("on key { : -> OpenCommandPalette(); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(binding.key.node.keys, vec![key_char(':')]);
    }

    #[test]
    fn modifier_shorthand() {
        let config = parse_ok("on key { C-s -> Save(); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(binding.key.node.keys, vec![key_char('C'), key_char('s')]);
    }

    #[test]
    fn modifier_with_shift() {
        let config = parse_ok("on key { C-S-p -> CommandPalette(); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(
            binding.key.node.keys,
            vec![key_char('C'), key_char('S'), key_char('p')]
        );
    }

    #[test]
    fn long_modifier_names() {
        let config = parse_ok("on key { Ctrl-s -> Save(); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(
            binding.key.node.keys,
            vec![key_named("Ctrl"), key_char('s')]
        );
    }

    #[test]
    fn cmd_modifier() {
        let config = parse_ok("on key { Cmd-s -> Save(); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(binding.key.node.keys, vec![key_named("Cmd"), key_char('s')]);
    }

    #[test]
    fn named_keys() {
        let config = parse_ok("on key { Space -> SetMode(space); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(binding.key.node.keys, vec![key_named("Space")]);
    }

    #[test]
    fn escape_key() {
        let config = parse_ok("on key { Escape -> SetMode(normal); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(binding.key.node.keys, vec![key_named("Escape")]);
    }

    #[test]
    fn modifier_with_named_key() {
        let config = parse_ok("on key { S-Tab -> Outdent(); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(binding.key.node.keys, vec![key_char('S'), key_named("Tab")]);
    }

    #[test]
    fn settings_with_semicolons() {
        let config = parse_ok(
            r#"
            on init {
                font.size = 14;
                editor.tab_size = 4;
            }
            "#,
        );
        let block = &config.blocks[0].node;
        assert_eq!(block.statements.len(), 2);

        let setting1 = assert_setting(&block.statements[0]);
        assert_eq!(setting1.path.len(), 2);
        assert_eq!(setting1.path[0].node, "font");
        assert_eq!(setting1.path[1].node, "size");
        assert_eq!(setting1.value.node, Value::Number(14.0));

        let setting2 = assert_setting(&block.statements[1]);
        assert_eq!(setting2.path.len(), 2);
        assert_eq!(setting2.path[0].node, "editor");
        assert_eq!(setting2.path[1].node, "tab_size");
        assert_eq!(setting2.value.node, Value::Number(4.0));
    }

    #[test]
    fn float_values() {
        let config = parse_ok("on init { font.size = 14.5; }");
        let setting = assert_setting(&config.blocks[0].node.statements[0]);
        assert_eq!(setting.value.node, Value::Number(14.5));
    }

    #[test]
    fn string_values_quoted() {
        let config = parse_ok(r#"on key { mode == "normal" { j -> MoveDown(); } }"#);
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
        let config = parse_ok("on key { mode == normal { j -> MoveDown(); } }");
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
        let config = parse_ok("on init { platform == Platform::MacOS { cmd_as_meta = true; } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Eq(field, val) => {
                assert_eq!(field.node, "platform");
                assert_eq!(
                    val.node,
                    Value::Enum {
                        ty: "Platform".to_string(),
                        variant: "MacOS".to_string()
                    }
                );
            },
            _ => panic!("expected Eq predicate"),
        }
    }

    #[test]
    fn array_values() {
        let config = parse_ok("on init { editor.rulers = [79, 100]; }");
        let setting = assert_setting(&config.blocks[0].node.statements[0]);
        match &setting.value.node {
            Value::Array(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].node, Value::Number(79.0));
                assert_eq!(items[1].node, Value::Number(100.0));
            },
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn predicate_equality() {
        let config = parse_ok(r#"on key { mode == "normal" { j -> MoveDown(); } }"#);
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
        let config = parse_ok(r#"on key { focus == "TextEditor" { C-s -> Save(); } }"#);
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
        let config = parse_ok("on key { cursor_line > 1 { k -> MoveUp(); } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Gt(field, val) => {
                assert_eq!(field.node, "cursor_line");
                assert_eq!(val.node, Value::Number(1.0));
            },
            _ => panic!("expected Gt predicate"),
        }

        let config = parse_ok("on key { cursor_line < 10 { j -> MoveDown(); } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Lt(field, val) => {
                assert_eq!(field.node, "cursor_line");
                assert_eq!(val.node, Value::Number(10.0));
            },
            _ => panic!("expected Lt predicate"),
        }

        let config = parse_ok("on key { cursor_line >= 1 { k -> MoveUp(); } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Gte(field, val) => {
                assert_eq!(field.node, "cursor_line");
                assert_eq!(val.node, Value::Number(1.0));
            },
            _ => panic!("expected Gte predicate"),
        }

        let config = parse_ok("on key { cursor_line <= 10 { j -> MoveDown(); } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Lte(field, val) => {
                assert_eq!(field.node, "cursor_line");
                assert_eq!(val.node, Value::Number(10.0));
            },
            _ => panic!("expected Lte predicate"),
        }
    }

    #[test]
    fn predicate_and() {
        let config = parse_ok(r#"on key { mode == "normal" && has_selection { d -> Delete(); } }"#);
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
                    Predicate::Bool(name) => assert_eq!(name.node, "has_selection"),
                    _ => panic!("expected Bool predicate on right"),
                }
            },
            _ => panic!("expected And predicate"),
        }
    }

    #[test]
    fn predicate_or() {
        let config =
            parse_ok(r#"on key { mode == "normal" || mode == "visual" { y -> Yank(); } }"#);
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
    fn bool_predicate_block() {
        let config = parse_ok("on key { has_selection { d -> Cut(); } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Bool(name) => assert_eq!(name.node, "has_selection"),
            _ => panic!("expected Bool predicate"),
        }
    }

    #[test]
    fn predicate_grouping() {
        let config = parse_ok(
            r#"on key { (mode == "normal" || mode == "visual") && cursor_line > 1 { k -> MoveUp(); } }"#,
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
                        assert_eq!(val.node, Value::Number(1.0));
                    },
                    _ => panic!("expected Gt predicate on right"),
                }
            },
            _ => panic!("expected And predicate"),
        }
    }

    #[test]
    fn predicate_matches() {
        let config = parse_ok(r#"on buffer { path ~ "*.rs" { rust_mode = true; } }"#);
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
                        j -> MoveDown();
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
        let config = parse_ok("on key { h -> MoveCursor(left); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        match &binding.action.node {
            ActionExpr::Single(action) => {
                assert_eq!(action.name, "MoveCursor");
                assert_eq!(action.args.len(), 1);
                match &action.args[0].node {
                    Arg::Positional(val) => {
                        assert_eq!(val.node, Value::Ident("left".to_string()));
                    },
                    _ => panic!("expected positional arg"),
                }
            },
            _ => panic!("expected single action"),
        }
    }

    #[test]
    fn action_with_named_args() {
        let config = parse_ok("on key { h -> MoveCursor(direction: left, count: 1); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        match &binding.action.node {
            ActionExpr::Single(action) => {
                assert_eq!(action.name, "MoveCursor");
                assert_eq!(action.args.len(), 2);
                match &action.args[0].node {
                    Arg::Named { name, value } => {
                        assert_eq!(name.node, "direction");
                        assert_eq!(value.node, Value::Ident("left".to_string()));
                    },
                    _ => panic!("expected named arg"),
                }
                match &action.args[1].node {
                    Arg::Named { name, value } => {
                        assert_eq!(name.node, "count");
                        assert_eq!(value.node, Value::Number(1.0));
                    },
                    _ => panic!("expected named arg"),
                }
            },
            _ => panic!("expected single action"),
        }
    }

    #[test]
    fn state_refs() {
        let config = parse_ok("on key { j -> MoveDown($count); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        match &binding.action.node {
            ActionExpr::Single(action) => match &action.args[0].node {
                Arg::Positional(val) => {
                    assert_eq!(val.node, Value::StateRef("count".to_string()));
                },
                _ => panic!("expected positional state ref"),
            },
            _ => panic!("expected single action"),
        }
    }

    #[test]
    fn action_sequences() {
        let config = parse_ok("on key { C-k -> [SelectLine(), Comment()]; }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        match &binding.action.node {
            ActionExpr::Sequence(actions) => {
                assert_eq!(actions.len(), 2);
                assert_eq!(actions[0].node.name, "SelectLine");
                assert_eq!(actions[1].node.name, "Comment");
            },
            _ => panic!("expected sequence"),
        }
    }

    #[test]
    fn let_bindings() {
        let config = parse_ok("on init { let leader = space; }");
        let let_binding = assert_let(&config.blocks[0].node.statements[0]);
        assert_eq!(let_binding.name.node, "leader");
        match &let_binding.value.node {
            Expr::Value(Value::Ident(val)) => assert_eq!(val, "space"),
            _ => panic!("expected ident value"),
        }
    }

    #[test]
    fn if_expressions() {
        let config =
            parse_ok(r#"on init { let mod = if platform == "macos" then cmd else ctrl; }"#);
        let let_binding = assert_let(&config.blocks[0].node.statements[0]);
        assert_eq!(let_binding.name.node, "mod");
        match &let_binding.value.node {
            Expr::If {
                condition,
                then_expr,
                else_expr,
            } => {
                match &condition.node {
                    Predicate::Eq(field, val) => {
                        assert_eq!(field.node, "platform");
                        assert_eq!(val.node, Value::String("macos".to_string()));
                    },
                    _ => panic!("expected Eq predicate"),
                }
                match &then_expr.node {
                    Expr::Value(Value::Ident(s)) => assert_eq!(s, "cmd"),
                    _ => panic!("expected ident in then branch"),
                }
                match &else_expr.node {
                    Expr::Value(Value::Ident(s)) => assert_eq!(s, "ctrl"),
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
                    h -> MoveCursor(left);
                    j -> MoveCursor(down);
                    k -> MoveCursor(up);
                    l -> MoveCursor(right);
                }
            }
            "#,
        );
        let fn_decl = assert_fn_decl(&config.blocks[0].node.statements[0]);
        assert_eq!(fn_decl.name.node, "vim_motions");
        assert_eq!(fn_decl.body.len(), 4);
    }

    #[test]
    fn comments() {
        let config = parse_ok(
            r#"
            # This is a comment
            on key {
                C-s -> Save();  # Inline comment
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
                C-s -> Save();
                invalid syntax here @@@
                C-q -> Quit();
            }
        "#;
        let (result, errors) = parse(source);
        assert!(!errors.is_empty(), "expected parse errors");
        let _ = result;
    }

    #[test]
    fn negative_numbers() {
        let config = parse_ok("on key { offset > -10 { k -> MoveUp(); } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Gt(field, val) => {
                assert_eq!(field.node, "offset");
                assert_eq!(val.node, Value::Number(-10.0));
            },
            _ => panic!("expected Gt predicate"),
        }
    }

    #[test]
    fn format_errors_output() {
        let source = "on key { C-s -> @@@ }";
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
                font.size = 14;
                let leader = space;
            }
            on buffer {
                path ~ "*.rs" {
                    rust_mode = true;
                }
            }
            on key {
                C-s -> Save();
                focus == "TextEditor" {
                    j -> MoveDown();
                    k -> MoveUp();
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
        let config = parse_ok("on init { read_only == true { C-s -> Noop(); } }");
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
        let config = parse_ok(r#"on key { mode != "insert" { j -> MoveDown(); } }"#);
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::NotEq(field, val) => {
                assert_eq!(field.node, "mode");
                assert_eq!(val.node, Value::String("insert".to_string()));
            },
            _ => panic!("expected NotEq predicate"),
        }
    }

    #[test]
    fn state_ref_in_value() {
        let config = parse_ok("on init { count = $register_count; }");
        let setting = assert_setting(&config.blocks[0].node.statements[0]);
        assert_eq!(
            setting.value.node,
            Value::StateRef("register_count".to_string())
        );
    }

    #[test]
    fn fn_call_statement() {
        let config = parse_ok("on key { vim_motions(); }");
        match &config.blocks[0].node.statements[0].node {
            Statement::FnCall(name) => assert_eq!(name.node, "vim_motions"),
            _ => panic!("expected fn call"),
        }
    }

    #[test]
    fn chord_keys() {
        let config = parse_ok("on key { h-j -> Diagonal(); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(binding.key.node.keys, vec![key_char('h'), key_char('j')]);
    }

    #[test]
    fn f_keys() {
        let config = parse_ok("on key { F1 -> Help(); }");
        let binding = assert_binding(&config.blocks[0].node.statements[0]);
        assert_eq!(binding.key.node.keys, vec![key_named("F1")]);
    }
}
