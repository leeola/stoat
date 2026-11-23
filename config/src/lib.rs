mod ast;
mod error;
mod parser;
mod schema;
mod settings;
mod template;

pub use ast::{
    Action, ActionExpr, Arg, Binding, Call, CallArg, CallValue, CommandBody, CommandDecl,
    CommandStatement, Config, EventBlock, EventType, Expr, FnDecl, Key, KeyPart, LetBinding,
    Predicate, PredicateBlock, RunExpr, Setting, Span, Spanned, Statement, ThemeBlock, Value,
};
pub use error::{format_errors, ParseError};
pub use schema::{settings_schema, PathSeg, SettingDef, ValueShape};
pub use settings::{
    LineNumbers, MinimapMode, MouseCapturePolicy, RepeatButton, Settings, TabBarMode, WrapMode,
};
pub use template::Template;

/// Parse a config source into its items, and the errors of the items that do
/// not parse.
///
/// A source that parses answers `(Some(config), [])`. A source whose only
/// errors sit in `command` items answers `(Some(config), errors)`. Its config
/// holds every other item and no entry for a bad command. A source with any
/// other error answers `(None, errors)`.
pub fn parse(source: &str) -> (Option<Config>, Vec<ParseError>) {
    parser::parse(source)
}

pub fn parse_action(source: &str) -> Result<Action, Vec<ParseError>> {
    parser::parse_action(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The five items of the CmdSpec board, one on each line, with each named
    /// argument in the colon syntax.
    const COMMAND_BOARD: &str = concat!(
        r#"command pr { doc = "check out a pull request"; "#,
        r#"arg pr = pick("gh pr list --json number,title,headRefName,state", "#,
        r##"show: "#{number} {title}", value: "{number}", "##,
        r#"preview: "gh pr view {number}"); "#,
        r#"run = shell("gh pr checkout {pr}"); output = notify; }"#,
        "\n",
        r#"command review { doc = "open a PR as diff-changes against its base"; "#,
        r#"arg pr = same_as(pr); "#,
        r#"run = [shell("gh pr checkout {pr}"), DiffFiles(rev: "origin/main")]; }"#,
        "\n",
        r#"command rg = cli("rg", output: locations);"#,
        "\n",
        r#"command jj = cli("jj", completions: "jj util completion fish");"#,
        "\n",
        r#"command ship { arg message = text(default: git.last_subject); "#,
        r#"run = [shell("cargo test"), shell("git push -u origin HEAD"), "#,
        r#"shell("gh pr create --fill --title {message}"), Notify("shipped")]; "#,
        r#"output = run_pane; }"#,
    );

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
            _ => panic!("expected binding, got {stmt:?}"),
        }
    }

    fn assert_predicate_block(stmt: &Spanned<Statement>) -> &PredicateBlock {
        match &stmt.node {
            Statement::PredicateBlock(s) => s,
            _ => panic!("expected predicate block, got {stmt:?}"),
        }
    }

    fn assert_setting(stmt: &Spanned<Statement>) -> &Setting {
        match &stmt.node {
            Statement::Setting(s) => s,
            _ => panic!("expected setting, got {stmt:?}"),
        }
    }

    fn assert_let(stmt: &Spanned<Statement>) -> &LetBinding {
        match &stmt.node {
            Statement::Let(l) => l,
            _ => panic!("expected let binding, got {stmt:?}"),
        }
    }

    fn assert_fn_decl(stmt: &Spanned<Statement>) -> &FnDecl {
        match &stmt.node {
            Statement::FnDecl(f) => f,
            _ => panic!("expected fn decl, got {stmt:?}"),
        }
    }

    fn key_char(c: char) -> Key {
        Key::Char(c)
    }

    fn key_named(s: &str) -> Key {
        Key::Named(s.to_string())
    }

    /// A source that fails still reports what it always reported.
    ///
    /// The parse recognizes first under an extra whose errors hold nothing, so
    /// only the second pass has a message at all. A message that read
    /// "unexpected" where it used to name what was expected would be this
    /// change leaking into a diagnostic.
    #[test]
    fn a_malformed_source_names_what_it_expected() {
        let (config, errors) = parse("on init { editor.wrap = }\n");

        assert!(config.is_none(), "the source does not parse");
        let message = errors
            .first()
            .map(|error| error.message.clone())
            .expect("a failure reports at least one error");
        assert!(
            message.starts_with("expected ") && message.contains(", found "),
            "the message names what was expected, not only what was found: {message:?}",
        );
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

    /// A string literal after the action names the binding, after one action
    /// or a sequence. A binding without one has no label, and the quote key
    /// still binds, since a label only follows an action.
    #[test]
    fn a_trailing_string_labels_the_binding() {
        let config = parse_ok(
            r#"on key {
                p -> OpenFileFinder() "files";
                r -> [ForceReloadAll(), SetMode(normal)]  "reload all";
                " -> SetMode(space_clipboard);
            }"#,
        );
        let labels: Vec<_> = config.blocks[0]
            .node
            .statements
            .iter()
            .map(|stmt| {
                let binding = assert_binding(stmt);
                binding.label.as_ref().map(|label| label.node.as_str())
            })
            .collect();
        assert_eq!(labels, [Some("files"), Some("reload all"), None]);
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
    fn predicate_display_round_trips_source() {
        let source_of = |predicate: &str| {
            let config = parse_ok(&format!("on key {{ {predicate} {{ j -> MoveDown(); }} }}"));
            assert_predicate_block(&config.blocks[0].node.statements[0])
                .predicate
                .node
                .to_string()
        };

        assert_eq!(source_of("mode == normal"), "mode == normal");
        assert_eq!(
            source_of("!token_known || token == function"),
            "!token_known || token == function"
        );
        assert_eq!(source_of(r#"lang ~ "rust*""#), r#"lang ~ "rust*""#);
        assert_eq!(source_of("a && (b || c)"), "a && (b || c)");
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
    fn predicate_negation() {
        let config = parse_ok("on key { !flag { d -> Cut(); } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Not(inner) => match &inner.node {
                Predicate::Bool(name) => assert_eq!(name.node, "flag"),
                _ => panic!("expected Bool inside Not"),
            },
            _ => panic!("expected Not predicate"),
        }
    }

    #[test]
    fn predicate_negation_binds_tighter_than_and() {
        let config = parse_ok(r#"on key { !(a || b) && c { k -> MoveUp(); } }"#);
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::And(left, right) => {
                match &left.node {
                    Predicate::Not(inner) => {
                        assert!(matches!(inner.node, Predicate::Or(_, _)), "!(a || b)")
                    },
                    _ => panic!("expected Not(Or) on the left"),
                }
                assert!(
                    matches!(right.node, Predicate::Bool(_)),
                    "bare c on the right"
                );
            },
            _ => panic!("expected And at the top"),
        }
    }

    #[test]
    fn predicate_double_negation() {
        let config = parse_ok("on key { !!flag { d -> Cut(); } }");
        let block = assert_predicate_block(&config.blocks[0].node.statements[0]);
        match &block.predicate.node {
            Predicate::Not(outer) => match &outer.node {
                Predicate::Not(inner) => {
                    assert!(matches!(inner.node, Predicate::Bool(_)), "!!flag")
                },
                _ => panic!("expected Not(Not(...))"),
            },
            _ => panic!("expected outer Not"),
        }
    }

    #[test]
    fn negation_key_binding_coexists_with_negation_block() {
        let config = parse_ok("on key { ! -> OpenHelp(); !flag { d -> Cut(); } }");
        let stmts = &config.blocks[0].node.statements;
        let binding = assert_binding(&stmts[0]);
        assert_eq!(binding.key.node.keys, vec![key_char('!')]);
        let block = assert_predicate_block(&stmts[1]);
        assert!(matches!(block.predicate.node, Predicate::Not(_)));
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

    #[test]
    fn theme_block_empty() {
        let config = parse_ok("theme default_dark { }");
        assert_eq!(config.blocks.len(), 0);
        assert_eq!(config.themes.len(), 1);
        assert_eq!(config.themes[0].node.name.node, "default_dark");
        assert_eq!(config.themes[0].node.parent, None);
        assert_eq!(config.themes[0].node.statements.len(), 0);
    }

    #[test]
    fn theme_block_with_inherits() {
        let config = parse_ok("theme mine inherits default_dark { ui.cursor.fg = red; }");
        let theme = &config.themes[0].node;
        assert_eq!(theme.name.node, "mine");
        assert_eq!(
            theme.parent.as_ref().map(|p| p.node.as_str()),
            Some("default_dark")
        );
        assert_eq!(theme.statements.len(), 1);
    }

    /// A segment after a dot takes a leading digit, so a scope carries an
    /// index in both the dotted form and the map form.
    #[test]
    fn a_setting_path_segment_takes_a_leading_digit() {
        let config =
            parse_ok(r##"theme t { vcs.lane.1.fg = "#7aa2f7"; vcs.lane.2 = { fg: red }; }"##);
        let paths: Vec<Vec<String>> = config.themes[0]
            .node
            .statements
            .iter()
            .map(|stmt| {
                assert_setting(stmt)
                    .path
                    .iter()
                    .map(|segment| segment.node.clone())
                    .collect()
            })
            .collect();
        assert_eq!(
            paths,
            [vec!["vcs", "lane", "1", "fg"], vec!["vcs", "lane", "2"]],
        );
    }

    #[test]
    fn theme_block_with_palette_lets() {
        let config = parse_ok(
            r##"theme default_dark {
                let bg = "#1e1e2e";
                let accent = "#89b4fa";
            }"##,
        );
        let theme = &config.themes[0].node;
        assert_eq!(theme.name.node, "default_dark");
        let bg = assert_let(&theme.statements[0]);
        assert_eq!(bg.name.node, "bg");
        match &bg.value.node {
            Expr::Value(Value::String(s)) => assert_eq!(s, "#1e1e2e"),
            _ => panic!("expected string value"),
        }
        let accent = assert_let(&theme.statements[1]);
        assert_eq!(accent.name.node, "accent");
    }

    #[test]
    fn theme_block_with_dotted_path_settings() {
        let config = parse_ok(
            r#"theme default_dark {
                ui.border.focused.fg = accent;
            }"#,
        );
        let theme = &config.themes[0].node;
        let setting = assert_setting(&theme.statements[0]);
        assert_eq!(
            setting
                .path
                .iter()
                .map(|p| p.node.as_str())
                .collect::<Vec<_>>(),
            vec!["ui", "border", "focused", "fg"]
        );
        assert_eq!(setting.value.node, Value::Ident("accent".to_string()));
    }

    #[test]
    fn map_value_basic() {
        let config = parse_ok(
            r#"theme default_dark {
                ui.cursor = { fg: bg, bg: accent };
            }"#,
        );
        let theme = &config.themes[0].node;
        let setting = assert_setting(&theme.statements[0]);
        match &setting.value.node {
            Value::Map(entries) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].0.node, "fg");
                assert_eq!(entries[0].1.node, Value::Ident("bg".to_string()));
                assert_eq!(entries[1].0.node, "bg");
                assert_eq!(entries[1].1.node, Value::Ident("accent".to_string()));
            },
            _ => panic!("expected map value"),
        }
    }

    #[test]
    fn map_value_with_modifiers_array() {
        let config = parse_ok(
            r#"theme default_dark {
                syntax.keyword = { fg: blue, modifiers: [bold, italic] };
            }"#,
        );
        let theme = &config.themes[0].node;
        let setting = assert_setting(&theme.statements[0]);
        match &setting.value.node {
            Value::Map(entries) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[1].0.node, "modifiers");
                match &entries[1].1.node {
                    Value::Array(items) => {
                        assert_eq!(items.len(), 2);
                        assert_eq!(items[0].node, Value::Ident("bold".to_string()));
                        assert_eq!(items[1].node, Value::Ident("italic".to_string()));
                    },
                    _ => panic!("expected array for modifiers"),
                }
            },
            _ => panic!("expected map value"),
        }
    }

    #[test]
    fn map_value_with_hex_string() {
        let config = parse_ok(
            r##"theme default_dark {
                ui.selection = { bg: "#45475a" };
            }"##,
        );
        let theme = &config.themes[0].node;
        let setting = assert_setting(&theme.statements[0]);
        match &setting.value.node {
            Value::Map(entries) => {
                assert_eq!(entries[0].0.node, "bg");
                assert_eq!(entries[0].1.node, Value::String("#45475a".to_string()));
            },
            _ => panic!("expected map value"),
        }
    }

    #[test]
    fn map_value_empty() {
        let config = parse_ok("theme t { ui.x = { }; }");
        let setting = assert_setting(&config.themes[0].node.statements[0]);
        assert_eq!(setting.value.node, Value::Map(vec![]));
    }

    #[test]
    fn map_value_trailing_comma() {
        let config = parse_ok(r#"theme t { ui.x = { fg: bg, bg: accent, }; }"#);
        let setting = assert_setting(&config.themes[0].node.statements[0]);
        match &setting.value.node {
            Value::Map(entries) => assert_eq!(entries.len(), 2),
            _ => panic!("expected map"),
        }
    }

    #[test]
    fn theme_blocks_and_event_blocks_coexist() {
        let config = parse_ok(
            r#"
            theme default_dark {
                ui.cursor.bg = accent;
            }
            on init {
                theme = default_dark;
            }
            on key {
                q -> Quit();
            }
            "#,
        );
        assert_eq!(config.themes.len(), 1);
        assert_eq!(config.themes[0].node.name.node, "default_dark");
        assert_eq!(config.blocks.len(), 2);
        assert_eq!(config.blocks[0].node.event, EventType::Init);
        assert_eq!(config.blocks[1].node.event, EventType::Key);
    }

    #[test]
    fn multiple_theme_blocks_same_name() {
        let config = parse_ok(
            r##"
            theme default_dark {
                ui.cursor.bg = "#89b4fa";
            }
            theme default_dark {
                ui.cursor.bg = "#ff0000";
            }
            "##,
        );
        assert_eq!(config.themes.len(), 2);
        assert_eq!(config.themes[0].node.name.node, "default_dark");
        assert_eq!(config.themes[1].node.name.node, "default_dark");
    }

    #[test]
    fn theme_block_mixed_forms() {
        let config = parse_ok(
            r##"theme t {
                let accent = "#89b4fa";
                ui.cursor = { fg: "black", bg: accent };
                ui.border.focused.fg = accent;
                syntax.comment = { fg: "#6c7086", modifiers: [italic] };
            }"##,
        );
        let theme = &config.themes[0].node;
        assert_eq!(theme.statements.len(), 4);
        assert_let(&theme.statements[0]);
        assert_setting(&theme.statements[1]);
        assert_setting(&theme.statements[2]);
        assert_setting(&theme.statements[3]);
    }

    /// The five items of the CmdSpec board parse to their declarations, with
    /// each named argument in the colon syntax.
    #[test]
    fn the_command_spec_board_parses_in_the_colon_syntax() {
        let src = COMMAND_BOARD;
        let lines: Vec<&str> = src.lines().collect();
        let pick = concat!(
            r#"pick("gh pr list --json number,title,headRefName,state", "#,
            r##"show: "#{number} {title}", value: "{number}", "##,
            r#"preview: "gh pr view {number}")"#,
        );
        let checkout = r#"shell("gh pr checkout {pr}")"#;
        let pr = decl_at(
            src,
            lines[0],
            0,
            CommandBody::Block(vec![
                doc_at(src, "check out a pull request"),
                arg_at(
                    src,
                    &format!("arg pr = {pick};"),
                    0,
                    call_at(
                        src,
                        pick,
                        0,
                        Some(vec![
                            positional(string_at(
                                src,
                                "gh pr list --json number,title,headRefName,state",
                                0,
                            )),
                            named("show", string_at(src, "#{number} {title}", 0)),
                            named("value", string_at(src, "{number}", 0)),
                            named("preview", string_at(src, "gh pr view {number}", 0)),
                        ]),
                    ),
                ),
                statement_at(
                    src,
                    &format!("run = {checkout};"),
                    0,
                    CommandStatement::Run(RunExpr::Single(shell_at(src, "gh pr checkout {pr}", 0))),
                ),
                statement_at(
                    src,
                    "output = notify;",
                    0,
                    CommandStatement::Output(word(src, "notify", 0)),
                ),
            ]),
        );

        let same_as = {
            let span = at(src, "same_as(pr)", 0);
            let value = span.start + "same_as(".len();
            let pr = CallValue::Value(Value::Ident("pr".to_owned()));
            let arg = positional(Spanned::new(pr, value..value + "pr".len()));
            Spanned::new(
                Call {
                    name: head("same_as", span.start),
                    args: Some(vec![arg]),
                },
                span,
            )
        };
        let diff_files = r#"DiffFiles(rev: "origin/main")"#;
        let review = decl_at(
            src,
            lines[1],
            0,
            CommandBody::Block(vec![
                doc_at(src, "open a PR as diff-changes against its base"),
                arg_at(src, "arg pr = same_as(pr);", 0, same_as),
                statement_at(
                    src,
                    &format!("run = [{checkout}, {diff_files}];"),
                    0,
                    CommandStatement::Run(RunExpr::Sequence(vec![
                        shell_at(src, "gh pr checkout {pr}", 1),
                        call_at(
                            src,
                            diff_files,
                            0,
                            Some(vec![named("rev", string_at(src, "origin/main", 0))]),
                        ),
                    ])),
                ),
            ]),
        );

        let locations = Spanned::new(
            CallValue::Value(Value::Ident("locations".to_owned())),
            at(src, "locations", 0),
        );
        let rg = decl_at(
            src,
            lines[2],
            0,
            CommandBody::Wrap(call_at(
                src,
                r#"cli("rg", output: locations)"#,
                0,
                Some(vec![
                    positional(string_at(src, "rg", 0)),
                    named("output", locations),
                ]),
            )),
        );
        let jj = decl_at(
            src,
            lines[3],
            0,
            CommandBody::Wrap(call_at(
                src,
                r#"cli("jj", completions: "jj util completion fish")"#,
                0,
                Some(vec![
                    positional(string_at(src, "jj", 0)),
                    named("completions", string_at(src, "jj util completion fish", 0)),
                ]),
            )),
        );

        let last_subject = {
            let span = at(src, "git.last_subject", 0);
            let segments = vec![
                head("git", span.start),
                head("last_subject", span.start + "git.".len()),
            ];
            Spanned::new(CallValue::Context(segments), span)
        };
        let ship_steps = [
            "cargo test",
            "git push -u origin HEAD",
            "gh pr create --fill --title {message}",
        ];
        let notify = r#"Notify("shipped")"#;
        let mut steps: Vec<Spanned<Call>> = ship_steps
            .iter()
            .map(|command| shell_at(src, command, 0))
            .collect();
        steps.push(call_at(
            src,
            notify,
            0,
            Some(vec![positional(string_at(src, "shipped", 0))]),
        ));
        let run_text = format!(
            "run = [{}, {notify}];",
            ship_steps
                .map(|command| format!("shell(\"{command}\")"))
                .join(", "),
        );
        let ship = decl_at(
            src,
            lines[4],
            0,
            CommandBody::Block(vec![
                arg_at(
                    src,
                    "arg message = text(default: git.last_subject);",
                    0,
                    call_at(
                        src,
                        "text(default: git.last_subject)",
                        0,
                        Some(vec![named("default", last_subject)]),
                    ),
                ),
                statement_at(
                    src,
                    &run_text,
                    0,
                    CommandStatement::Run(RunExpr::Sequence(steps)),
                ),
                statement_at(
                    src,
                    "output = run_pane;",
                    0,
                    CommandStatement::Output(word(src, "run_pane", 0)),
                ),
            ]),
        );

        assert_eq!(parse_ok(src).commands, [pr, review, rg, jj, ship]);
    }

    /// A bare name holds no argument list, and a name with empty parentheses
    /// holds an empty one.
    #[test]
    fn a_bare_name_and_an_empty_call_differ() {
        let src = "command a { arg f = file; run = [b, OpenRun()]; }";
        let run = CommandStatement::Run(RunExpr::Sequence(vec![
            call_at(src, "b", 0, None),
            call_at(src, "OpenRun()", 0, Some(Vec::new())),
        ]));
        assert_eq!(
            parse_ok(src).commands,
            [decl_at(
                src,
                src,
                0,
                CommandBody::Block(vec![
                    arg_at(src, "arg f = file;", 0, call_at(src, "file", 0, None)),
                    statement_at(src, "run = [b, OpenRun()];", 0, run),
                ]),
            )],
        );
    }

    /// A one-word value of a call stays an identifier. Two words joined by
    /// `.` read as a context reference with a span for each segment.
    #[test]
    fn a_one_word_value_is_an_identifier_and_a_dotted_value_is_a_context_reference() {
        let src = concat!(
            "command a { arg x = text(default: selection); ",
            "arg y = text(default: git.last_subject); }",
        );
        let selection = Spanned::new(
            CallValue::Value(Value::Ident("selection".to_owned())),
            at(src, "selection", 0),
        );
        let context = Spanned::new(
            CallValue::Context(vec![word(src, "git", 0), word(src, "last_subject", 0)]),
            at(src, "git.last_subject", 0),
        );
        let text_of =
            |text: &str, value| call_at(src, text, 0, Some(vec![named("default", value)]));
        assert_eq!(
            parse_ok(src).commands,
            [decl_at(
                src,
                src,
                0,
                CommandBody::Block(vec![
                    arg_at(
                        src,
                        "arg x = text(default: selection);",
                        0,
                        text_of("text(default: selection)", selection),
                    ),
                    arg_at(
                        src,
                        "arg y = text(default: git.last_subject);",
                        0,
                        text_of("text(default: git.last_subject)", context),
                    ),
                ]),
            )],
        );
    }

    /// `command` opens an item only at the top level. Inside a block it stays
    /// an identifier, as a setting path segment and as an argument name.
    #[test]
    fn command_stays_an_identifier_inside_a_block() {
        let config = parse_ok(concat!(
            r#"on init { lsp.command.rust = "ra"; command = 1; } "#,
            r#"on key { r -> Run(command: "make"); }"#,
        ));
        let paths: Vec<Vec<String>> = config.blocks[0]
            .node
            .statements
            .iter()
            .map(|statement| {
                let path = &assert_setting(statement).path;
                path.iter().map(|segment| segment.node.clone()).collect()
            })
            .collect();
        let binding = assert_binding(&config.blocks[1].node.statements[0]);
        let argument = match &binding.action.node {
            ActionExpr::Single(action) => match &action.args[0].node {
                Arg::Named { name, .. } => name.node.clone(),
                other => panic!("expected a named argument, got {other:?}"),
            },
            other => panic!("expected one action, got {other:?}"),
        };

        let segments = |words: &[&str]| words.iter().map(|word| word.to_string()).collect();
        assert_eq!(
            (config.commands.is_empty(), paths, argument),
            (
                true,
                vec![
                    segments(&["lsp", "command", "rust"]),
                    segments(&["command"])
                ],
                "command".to_owned(),
            ),
        );
    }

    /// A command item ends in `;` after its call, and a statement of its
    /// block ends in `;`. A source without either reports errors and yields
    /// no command.
    #[test]
    fn a_command_item_with_no_terminator_is_an_error() {
        let outcome = |source: &str| {
            let (config, errors) = parse(source);
            let no_command = config.is_none_or(|config| config.commands.is_empty());
            (!errors.is_empty(), no_command)
        };
        assert_eq!(
            [
                outcome(r#"command rg = cli("rg")"#),
                outcome(r#"command a { run = shell("x") }"#),
            ],
            [(true, true); 2],
        );
    }

    /// A bad command item between two other items reports one error at the
    /// token that broke it, and the items around it parse.
    #[test]
    fn a_broken_command_between_two_items_is_reported_and_skipped() {
        let source = concat!(
            "on init { editor.scrolloff = 3; }\n",
            "command broken { doc = ; }\n",
            "theme t { ui.text.fg = red; }",
        );
        let (config, errors) = parse(source);
        let config = config.expect("the other items parse");
        let semicolon = at(source, "= ;", 0).start + "= ".len();

        let items = (
            config.blocks.len(),
            config.themes.len(),
            config.commands.len(),
        );
        let spans: Vec<(usize, usize)> = errors
            .iter()
            .map(|error| (error.span.start, error.span.end))
            .collect();
        let expected = errors
            .iter()
            .all(|error| error.message.starts_with("expected "));
        assert_eq!(
            (items, spans, expected),
            ((1, 1, 0), vec![(semicolon, semicolon + 1)], true),
        );
    }

    /// A bad wrapped command ends at its semicolon, so the command after it
    /// parses. The error stands at the `=` where a named argument takes a
    /// colon.
    #[test]
    fn a_broken_wrap_command_recovers_at_its_semicolon() {
        let source = concat!(
            r#"command rg = cli("rg", output = locations);"#,
            "\n",
            r#"command jj = cli("jj");"#,
        );
        let (config, errors) = parse(source);
        let names: Vec<String> = config
            .expect("the second command parses")
            .commands
            .iter()
            .map(|command| command.node.name.node.clone())
            .collect();
        let equals = at(source, "= locations", 0).start;

        let spans: Vec<(usize, usize)> = errors
            .iter()
            .map(|error| (error.span.start, error.span.end))
            .collect();
        assert_eq!(
            (names, spans),
            (vec!["jj".to_owned()], vec![(equals, equals + 1)]),
        );
    }

    /// The end of a bad command reads past a `}` in a string and in a
    /// comment. The item then ends at the close of its brace group.
    #[test]
    fn recovery_reads_past_a_brace_in_a_string_and_in_a_comment() {
        let source = concat!(
            r#"command a { doc = "}"; # }"#,
            "\n",
            "run = ; }\n",
            "on key { q -> Quit(); }",
        );
        let (config, errors) = parse(source);
        let config = config.expect("the key block parses");
        assert_eq!(
            (config.blocks.len(), config.commands.len(), errors.len()),
            (1, 0, 1),
        );
    }

    /// A bad command with no end, such as an unclosed brace, fails the whole
    /// parse.
    #[test]
    fn a_command_with_no_end_fails_the_parse() {
        let (config, errors) = parse(concat!(
            r#"command a { run = shell("x");"#,
            "\n",
            "on key { q -> Quit(); }",
        ));
        assert_eq!((config.is_none(), errors.is_empty()), (true, false));
    }

    /// The span of the `nth` occurrence of `needle` in `source`, counting
    /// from 0.
    fn at(source: &str, needle: &str, nth: usize) -> Span {
        let (start, _) = source
            .match_indices(needle)
            .nth(nth)
            .expect("the needle occurs that often");
        start..start + needle.len()
    }

    /// `needle` spanned at its `nth` occurrence in `source`.
    fn word(source: &str, needle: &str, nth: usize) -> Spanned<String> {
        Spanned::new(needle.to_owned(), at(source, needle, nth))
    }

    /// The identifier that `text` starts with, spanned from `start`.
    fn head(text: &str, start: usize) -> Spanned<String> {
        let end = text
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(text.len());
        Spanned::new(text[..end].to_owned(), start..start + end)
    }

    /// The command item written as `text` at its `nth` occurrence in
    /// `source`, with `body`.
    fn decl_at(source: &str, text: &str, nth: usize, body: CommandBody) -> Spanned<CommandDecl> {
        let span = at(source, text, nth);
        let name_start = span.start + "command ".len();
        let name = head(&text["command ".len()..], name_start);
        Spanned::new(CommandDecl { name, body }, span)
    }

    /// The statement written as `text` at its `nth` occurrence in `source`.
    fn statement_at(
        source: &str,
        text: &str,
        nth: usize,
        node: CommandStatement,
    ) -> Spanned<CommandStatement> {
        Spanned::new(node, at(source, text, nth))
    }

    /// The `doc` statement of the description `text`, which occurs once in
    /// `source`.
    fn doc_at(source: &str, text: &str) -> Spanned<CommandStatement> {
        let quoted = format!("\"{text}\"");
        let doc = Spanned::new(text.to_owned(), at(source, &quoted, 0));
        statement_at(
            source,
            &format!("doc = {quoted};"),
            0,
            CommandStatement::Doc(doc),
        )
    }

    /// The `arg` statement written as `text` at its `nth` occurrence in
    /// `source`, with `kind`.
    fn arg_at(
        source: &str,
        text: &str,
        nth: usize,
        kind: Spanned<Call>,
    ) -> Spanned<CommandStatement> {
        let span = at(source, text, nth);
        let name = head(&text["arg ".len()..], span.start + "arg ".len());
        Spanned::new(CommandStatement::Arg { name, kind }, span)
    }

    /// The call written as `text` at its `nth` occurrence in `source`.
    fn call_at(
        source: &str,
        text: &str,
        nth: usize,
        args: Option<Vec<Spanned<CallArg>>>,
    ) -> Spanned<Call> {
        let span = at(source, text, nth);
        Spanned::new(
            Call {
                name: head(text, span.start),
                args,
            },
            span,
        )
    }

    /// The step `shell("command")` at its `nth` occurrence in `source`.
    fn shell_at(source: &str, command: &str, nth: usize) -> Spanned<Call> {
        let text = format!("shell(\"{command}\")");
        let arg = positional(string_at(source, command, nth));
        call_at(source, &text, nth, Some(vec![arg]))
    }

    /// The string `text` as a call value, spanned at the `nth` occurrence of
    /// its quoted form in `source`.
    fn string_at(source: &str, text: &str, nth: usize) -> Spanned<CallValue> {
        let span = at(source, &format!("\"{text}\""), nth);
        Spanned::new(CallValue::Value(Value::String(text.to_owned())), span)
    }

    /// A positional argument of `value`, spanned as the value is.
    fn positional(value: Spanned<CallValue>) -> Spanned<CallArg> {
        let span = value.span.clone();
        Spanned::new(CallArg::Positional(value), span)
    }

    /// The argument `name: value`, spanned from the name to the end of the
    /// value. One blank stands between the colon and the value.
    fn named(name: &str, value: Spanned<CallValue>) -> Spanned<CallArg> {
        let start = value.span.start - name.len() - ": ".len();
        let span = start..value.span.end;
        let name = head(name, start);
        Spanned::new(CallArg::Named { name, value }, span)
    }
}
