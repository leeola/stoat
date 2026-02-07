use crate::{
    ast::{
        Action, ActionExpr, Arg, Binding, Config, EventBlock, EventType, Expr, FnDecl, Key,
        KeyCombo, KeyPart, LetBinding, Predicate, PredicateBlock, Setting, Spanned, Statement,
        Value,
    },
    error::ParseError,
};
use chumsky::prelude::*;

fn comment() -> impl Parser<char, (), Error = Simple<char>> + Clone {
    just('#')
        .then(take_until(just('\n').or(end().to('\n'))))
        .padded()
        .ignored()
}

fn ws() -> impl Parser<char, (), Error = Simple<char>> + Clone {
    filter(|c: &char| c.is_whitespace())
        .ignored()
        .or(comment())
        .repeated()
        .ignored()
}

fn required_ws() -> impl Parser<char, (), Error = Simple<char>> + Clone {
    filter(|c: &char| c.is_whitespace())
        .repeated()
        .at_least(1)
        .ignored()
}

fn ident() -> impl Parser<char, String, Error = Simple<char>> + Clone {
    filter(|c: &char| c.is_ascii_alphabetic() || *c == '_')
        .chain(filter(|c: &char| c.is_ascii_alphanumeric() || *c == '_').repeated())
        .collect()
}

fn spanned_ident() -> impl Parser<char, Spanned<String>, Error = Simple<char>> + Clone {
    ident().map_with_span(Spanned::new)
}

fn string_literal() -> impl Parser<char, String, Error = Simple<char>> + Clone {
    just('"')
        .ignore_then(
            filter(|c: &char| *c != '"' && *c != '\\')
                .or(just('\\').ignore_then(any()))
                .repeated(),
        )
        .then_ignore(just('"'))
        .collect()
}

fn spanned_string_literal() -> impl Parser<char, Spanned<String>, Error = Simple<char>> + Clone {
    string_literal().map_with_span(Spanned::new)
}

fn number() -> impl Parser<char, f64, Error = Simple<char>> + Clone {
    just('-')
        .or_not()
        .chain::<char, _, _>(filter(|c: &char| c.is_ascii_digit()).repeated().at_least(1))
        .chain::<char, _, _>(
            just('.')
                .chain(filter(|c: &char| c.is_ascii_digit()).repeated().at_least(1))
                .or_not()
                .flatten(),
        )
        .collect::<String>()
        .try_map(|s, span| {
            s.parse::<f64>()
                .map_err(|_| Simple::custom(span, "invalid number"))
        })
}

fn enum_value() -> impl Parser<char, Value, Error = Simple<char>> + Clone {
    ident()
        .then_ignore(just("::"))
        .then(ident())
        .map(|(ty, variant)| Value::Enum { ty, variant })
}

fn array_value() -> impl Parser<char, Value, Error = Simple<char>> + Clone {
    recursive(|arr| {
        let inner_value = string_literal()
            .map(Value::String)
            .or(number().map(Value::Number))
            .or(arr)
            .or(ident().map(|s| match s.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => Value::Ident(s),
            }));

        just('[')
            .ignore_then(ws())
            .ignore_then(
                inner_value
                    .map_with_span(Spanned::new)
                    .separated_by(just(',').padded_by(ws()))
                    .allow_trailing(),
            )
            .then_ignore(ws())
            .then_ignore(just(']'))
            .map(Value::Array)
    })
}

fn value() -> impl Parser<char, Value, Error = Simple<char>> + Clone {
    let state_ref = just('$').ignore_then(ident()).map(Value::StateRef);

    string_literal()
        .map(Value::String)
        .or(enum_value())
        .or(number().map(Value::Number))
        .or(array_value())
        .or(state_ref)
        .or(ident().map(|s| match s.as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::Ident(s),
        }))
}

fn spanned_value() -> impl Parser<char, Spanned<Value>, Error = Simple<char>> + Clone {
    value().map_with_span(Spanned::new)
}

fn expr() -> impl Parser<char, Expr, Error = Simple<char>> + Clone {
    recursive(|expr| {
        let state_ref = just('$')
            .ignore_then(ident())
            .map(|s| Expr::Value(Value::StateRef(s)));

        let string_expr = string_literal().map(|s| Expr::Value(Value::String(s)));
        let num_expr = number().map(|n| Expr::Value(Value::Number(n)));
        let ident_expr = ident().map(|s| match s.as_str() {
            "true" => Expr::Value(Value::Bool(true)),
            "false" => Expr::Value(Value::Bool(false)),
            _ => Expr::Value(Value::Ident(s)),
        });

        let atom = state_ref.or(string_expr).or(num_expr).or(ident_expr);

        let with_default = atom
            .clone()
            .then(
                ws().ignore_then(just("??"))
                    .ignore_then(ws())
                    .ignore_then(atom.clone())
                    .or_not(),
            )
            .map(|(left, right)| match right {
                Some(default) => {
                    let left_val = match left {
                        Expr::Value(v) => v,
                        _ => return left,
                    };
                    // FIXME: Default expression not yet modeled in simplified Expr
                    let _ = default;
                    Expr::Value(left_val)
                },
                None => left,
            });

        let if_expr = just("if")
            .ignore_then(required_ws())
            .ignore_then(predicate_inner().map_with_span(Spanned::new))
            .then_ignore(ws())
            .then_ignore(just("then"))
            .then_ignore(required_ws())
            .then(expr.clone().map_with_span(Spanned::new))
            .then_ignore(ws())
            .then_ignore(just("else"))
            .then_ignore(required_ws())
            .then(expr.clone().map_with_span(Spanned::new))
            .map(|((cond, then_branch), else_branch)| Expr::If {
                condition: Box::new(cond),
                then_expr: Box::new(then_branch),
                else_expr: Box::new(else_branch),
            });

        if_expr.or(with_default)
    })
}

fn spanned_expr() -> impl Parser<char, Spanned<Expr>, Error = Simple<char>> + Clone {
    expr().map_with_span(Spanned::new)
}

const KEY_TERMINATORS: &[char] = &[' ', '{', '}', '\n', '\r', '\t', '#'];

fn is_key_char(c: &char) -> bool {
    !KEY_TERMINATORS.contains(c) && *c != '-'
}

fn key() -> impl Parser<char, Key, Error = Simple<char>> + Clone {
    ident()
        .try_map(|s, span| {
            if s.len() == 1 {
                s.chars()
                    .next()
                    .map(Key::Char)
                    .ok_or_else(|| Simple::custom(span, "empty key"))
            } else {
                Ok(Key::Named(s))
            }
        })
        .or(
            filter(|c: &char| is_key_char(c) && !c.is_ascii_alphabetic() && *c != '_')
                .map(Key::Char),
        )
}

fn key_part() -> impl Parser<char, KeyPart, Error = Simple<char>> + Clone {
    key()
        .separated_by(just('-'))
        .at_least(1)
        .map(|keys| KeyPart { keys })
}

fn key_combo() -> impl Parser<char, KeyCombo, Error = Simple<char>> + Clone {
    key_part()
        .separated_by(just(' ').repeated().at_least(1))
        .at_least(1)
        .map(|parts| KeyCombo { parts })
}

fn spanned_key_combo() -> impl Parser<char, Spanned<KeyCombo>, Error = Simple<char>> + Clone {
    key_combo().map_with_span(Spanned::new)
}

fn arg() -> impl Parser<char, Spanned<Arg>, Error = Simple<char>> + Clone {
    let named = spanned_ident()
        .then_ignore(ws())
        .then_ignore(just(':'))
        .then_ignore(ws())
        .then(spanned_value())
        .map(|(name, value)| Arg::Named { name, value });

    let positional = spanned_value().map(Arg::Positional);

    named.or(positional).map_with_span(Spanned::new)
}

fn action() -> impl Parser<char, Action, Error = Simple<char>> + Clone {
    ident()
        .then_ignore(ws())
        .then_ignore(just('('))
        .then_ignore(ws())
        .then(
            arg()
                .separated_by(just(',').padded_by(ws()))
                .allow_trailing(),
        )
        .then_ignore(ws())
        .then_ignore(just(')'))
        .map(|(name, args)| Action { name, args })
}

fn spanned_action() -> impl Parser<char, Spanned<Action>, Error = Simple<char>> + Clone {
    action().map_with_span(Spanned::new)
}

fn action_expr() -> impl Parser<char, ActionExpr, Error = Simple<char>> + Clone {
    let sequence = just('[')
        .ignore_then(ws())
        .ignore_then(
            spanned_action()
                .separated_by(just(',').padded_by(ws()))
                .allow_trailing(),
        )
        .then_ignore(ws())
        .then_ignore(just(']'))
        .map(ActionExpr::Sequence);

    sequence.or(action().map(ActionExpr::Single))
}

fn spanned_action_expr() -> impl Parser<char, Spanned<ActionExpr>, Error = Simple<char>> + Clone {
    action_expr().map_with_span(Spanned::new)
}

fn predicate_inner() -> impl Parser<char, Predicate, Error = Simple<char>> + Clone {
    recursive(|pred| {
        let parens = just('(')
            .ignore_then(ws())
            .ignore_then(pred.clone().map_with_span(Spanned::new))
            .then_ignore(ws())
            .then_ignore(just(')'));

        let matches = spanned_ident()
            .then_ignore(ws())
            .then_ignore(just('~'))
            .then_ignore(ws())
            .then(spanned_string_literal())
            .map(|(field, pattern)| Predicate::Matches(field, pattern));

        let comparison = spanned_ident()
            .then_ignore(ws())
            .then(
                just("==")
                    .or(just("!="))
                    .or(just(">="))
                    .or(just("<="))
                    .or(just(">"))
                    .or(just("<")),
            )
            .then_ignore(ws())
            .then(spanned_value())
            .map(|((field, op), val)| match op {
                "==" => Predicate::Eq(field, val),
                "!=" => Predicate::NotEq(field, val),
                ">" => Predicate::Gt(field, val),
                "<" => Predicate::Lt(field, val),
                ">=" => Predicate::Gte(field, val),
                "<=" => Predicate::Lte(field, val),
                _ => unreachable!(),
            });

        let var = spanned_ident().map(Predicate::Bool);

        let atom = parens
            .or(matches.map_with_span(Spanned::new))
            .or(comparison.map_with_span(Spanned::new))
            .or(var.map_with_span(Spanned::new));

        let and_chain = atom
            .clone()
            .then(
                ws().ignore_then(just("&&"))
                    .ignore_then(ws())
                    .ignore_then(atom.clone())
                    .repeated(),
            )
            .foldl(|left, right| {
                let span = left.span.start..right.span.end;
                Spanned::new(Predicate::And(Box::new(left), Box::new(right)), span)
            });

        and_chain
            .clone()
            .then(
                ws().ignore_then(just("||"))
                    .ignore_then(ws())
                    .ignore_then(and_chain.clone())
                    .repeated(),
            )
            .foldl(|left, right| {
                let span = left.span.start..right.span.end;
                Spanned::new(Predicate::Or(Box::new(left), Box::new(right)), span)
            })
            .map(|spanned| spanned.node)
    })
}

fn predicate() -> impl Parser<char, Spanned<Predicate>, Error = Simple<char>> + Clone {
    predicate_inner().map_with_span(Spanned::new)
}

fn setting() -> impl Parser<char, Setting, Error = Simple<char>> + Clone {
    spanned_ident()
        .then(just('.').ignore_then(spanned_ident()).repeated())
        .then_ignore(ws())
        .then_ignore(just('='))
        .then_ignore(ws())
        .then(spanned_value())
        .map(|((first, rest), value)| {
            let mut path = vec![first];
            path.extend(rest);
            Setting { path, value }
        })
}

fn binding() -> impl Parser<char, Binding, Error = Simple<char>> + Clone {
    spanned_key_combo()
        .then_ignore(ws())
        .then_ignore(just("->"))
        .then_ignore(ws())
        .then(spanned_action_expr())
        .map(|(key, action)| Binding { key, action })
}

fn let_stmt() -> impl Parser<char, LetBinding, Error = Simple<char>> + Clone {
    just("let")
        .ignore_then(required_ws())
        .ignore_then(spanned_ident())
        .then_ignore(ws())
        .then_ignore(just('='))
        .then_ignore(ws())
        .then(spanned_expr())
        .map(|(name, value)| LetBinding { name, value })
}

fn fn_decl(
    stmt: impl Parser<char, Spanned<Statement>, Error = Simple<char>> + Clone,
) -> impl Parser<char, FnDecl, Error = Simple<char>> + Clone {
    just("fn")
        .ignore_then(required_ws())
        .ignore_then(spanned_ident())
        .then_ignore(ws())
        .then_ignore(just('('))
        .then_ignore(ws())
        .then_ignore(just(')'))
        .then_ignore(ws())
        .then_ignore(just('{'))
        .then_ignore(ws())
        .then(stmt.repeated())
        .then_ignore(ws())
        .then_ignore(just('}'))
        .map(|(name, body)| FnDecl { name, body })
}

fn fn_call() -> impl Parser<char, Spanned<String>, Error = Simple<char>> + Clone {
    spanned_ident()
        .then_ignore(ws())
        .then_ignore(just('('))
        .then_ignore(ws())
        .then_ignore(just(')'))
}

fn predicate_block(
    stmt: impl Parser<char, Spanned<Statement>, Error = Simple<char>> + Clone,
) -> impl Parser<char, PredicateBlock, Error = Simple<char>> + Clone {
    predicate()
        .then_ignore(ws())
        .then_ignore(just('{'))
        .then_ignore(ws())
        .then(stmt.repeated())
        .then_ignore(ws())
        .then_ignore(just('}'))
        .map(|(predicate, body)| PredicateBlock { predicate, body })
}

fn semicolon() -> impl Parser<char, (), Error = Simple<char>> + Clone {
    ws().ignore_then(just(';')).ignored()
}

fn statement() -> impl Parser<char, Spanned<Statement>, Error = Simple<char>> + Clone {
    recursive(|stmt| {
        let fn_decl_stmt = fn_decl(stmt.clone()).map(Statement::FnDecl);
        let fn_call_stmt = fn_call().map(Statement::FnCall).then_ignore(semicolon());
        let let_binding = let_stmt().map(Statement::Let).then_ignore(semicolon());
        let predicate_block_stmt = predicate_block(stmt.clone()).map(Statement::PredicateBlock);
        let binding_stmt = binding().map(Statement::Binding).then_ignore(semicolon());
        let setting_stmt = setting().map(Statement::Setting).then_ignore(semicolon());

        fn_decl_stmt
            .or(fn_call_stmt)
            .or(let_binding)
            .or(predicate_block_stmt)
            .or(binding_stmt)
            .or(setting_stmt)
            .map_with_span(Spanned::new)
            .then_ignore(ws())
    })
}

fn event_type() -> impl Parser<char, EventType, Error = Simple<char>> + Clone {
    just("init")
        .to(EventType::Init)
        .or(just("buffer").to(EventType::Buffer))
        .or(just("key").to(EventType::Key))
}

fn event_block() -> impl Parser<char, Spanned<EventBlock>, Error = Simple<char>> + Clone {
    just("on")
        .ignore_then(required_ws())
        .ignore_then(event_type())
        .then_ignore(ws())
        .then_ignore(just('{'))
        .then_ignore(ws())
        .then(statement().repeated())
        .then_ignore(ws())
        .then_ignore(just('}'))
        .map(|(event, statements)| EventBlock { event, statements })
        .map_with_span(Spanned::new)
}

fn config() -> impl Parser<char, Config, Error = Simple<char>> {
    ws().ignore_then(event_block().padded_by(ws()).repeated())
        .then_ignore(end())
        .map(|blocks| Config { blocks })
}

pub fn parser() -> impl Parser<char, Config, Error = Simple<char>> {
    config()
}

pub fn parse(source: &str) -> (Option<Config>, Vec<ParseError>) {
    let (result, errs) = parser().parse_recovery(source);

    let errors = errs
        .into_iter()
        .map(|e| {
            let span = e.span();
            let message = match e.reason() {
                chumsky::error::SimpleReason::Unexpected => {
                    let found = e
                        .found()
                        .map(|c| format!("'{c}'"))
                        .unwrap_or_else(|| "end of input".to_string());
                    let expected: Vec<_> = e
                        .expected()
                        .filter_map(|exp| exp.as_ref().map(|c| format!("'{c}'")))
                        .collect();
                    if expected.is_empty() {
                        format!("unexpected {found}")
                    } else {
                        format!("expected {}, found {}", expected.join(" or "), found)
                    }
                },
                chumsky::error::SimpleReason::Unclosed { span: _, delimiter } => {
                    format!("unclosed delimiter '{delimiter}'")
                },
                chumsky::error::SimpleReason::Custom(msg) => msg.clone(),
            };
            ParseError::new(span, message)
        })
        .collect();

    (result, errors)
}
