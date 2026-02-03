use crate::{
    ast::{
        ActionCall, ActionExpr, Arg, Binding, Config, EventBlock, EventType, Expr, FnDecl,
        KeyCombo, KeyPart, LetBinding, Predicate, PredicateBlock, PrefixBlock, Setting, Spanned,
        Statement, Value,
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

fn number() -> impl Parser<char, (i64, Option<f64>), Error = Simple<char>> + Clone {
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
            if s.contains('.') {
                s.parse::<f64>()
                    .map(|f| (0, Some(f)))
                    .map_err(|_| Simple::custom(span, "invalid float"))
            } else {
                s.parse::<i64>()
                    .map(|i| (i, None))
                    .map_err(|_| Simple::custom(span, "invalid integer"))
            }
        })
}

fn enum_value() -> impl Parser<char, Value, Error = Simple<char>> + Clone {
    ident()
        .then_ignore(just("::"))
        .then(ident())
        .map(|(ty, variant)| Value::Enum(ty, variant))
}

fn array_value() -> impl Parser<char, Value, Error = Simple<char>> + Clone {
    recursive(|arr| {
        let inner_value = string_literal()
            .map(Value::String)
            .or(number().map(|(i, f)| f.map(Value::Float).unwrap_or(Value::Int(i))))
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
        .or(number().map(|(i, f)| f.map(Value::Float).unwrap_or(Value::Int(i))))
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
        let state_ref = just('$').ignore_then(ident()).map(Expr::StateRef);

        let string_expr = string_literal().map(Expr::String);
        let num_expr = number().map(|(i, f)| f.map(Expr::Float).unwrap_or(Expr::Int(i)));
        let ident_expr = ident().map(|s| match s.as_str() {
            "true" => Expr::Bool(true),
            "false" => Expr::Bool(false),
            _ => Expr::Ident(s),
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
                Some(default) => Expr::Default(Box::new(left), Box::new(default)),
                None => left,
            });

        let comparison = with_default
            .clone()
            .then(
                ws().ignore_then(just("==").or(just("!=")))
                    .then_ignore(ws())
                    .then(with_default.clone())
                    .or_not(),
            )
            .map(|(left, right)| match right {
                Some(("==", rhs)) => Expr::Eq(Box::new(left), Box::new(rhs)),
                Some(("!=", rhs)) => Expr::Ne(Box::new(left), Box::new(rhs)),
                Some(_) => unreachable!(),
                None => left,
            });

        let if_expr = just("if")
            .ignore_then(required_ws())
            .ignore_then(comparison.clone())
            .then_ignore(ws())
            .then_ignore(just("then"))
            .then_ignore(required_ws())
            .then(expr.clone())
            .then_ignore(ws())
            .then_ignore(just("else"))
            .then_ignore(required_ws())
            .then(expr.clone())
            .map(|((cond, then_branch), else_branch)| {
                Expr::If(Box::new(cond), Box::new(then_branch), Box::new(else_branch))
            });

        if_expr.or(comparison)
    })
}

fn spanned_expr() -> impl Parser<char, Spanned<Expr>, Error = Simple<char>> + Clone {
    expr().map_with_span(Spanned::new)
}

fn key_part() -> impl Parser<char, KeyPart, Error = Simple<char>> + Clone {
    let modifier = just("ctrl")
        .or(just("shift"))
        .or(just("alt"))
        .or(just("cmd"))
        .or(just("super"))
        .map(|s: &str| s.to_string());

    let modifier_with_plus = modifier.then_ignore(just('+')).repeated();

    modifier_with_plus
        .then(ident())
        .map(|(modifiers, key)| KeyPart { modifiers, key })
}

fn key_combo() -> impl Parser<char, KeyCombo, Error = Simple<char>> + Clone {
    key_part()
        .separated_by(just(' ').repeated().at_least(1))
        .at_least(1)
        .map(|keys| KeyCombo { keys })
}

fn spanned_key_combo() -> impl Parser<char, Spanned<KeyCombo>, Error = Simple<char>> + Clone {
    key_combo().map_with_span(Spanned::new)
}

fn arg() -> impl Parser<char, Arg, Error = Simple<char>> + Clone {
    ident()
        .then_ignore(ws())
        .then_ignore(just(':'))
        .then_ignore(ws())
        .then(expr())
        .map(|(name, value)| Arg::Named(name, value))
        .or(expr().map(Arg::Positional))
}

fn action_call() -> impl Parser<char, ActionCall, Error = Simple<char>> + Clone {
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
        .map(|(name, args)| ActionCall { name, args })
}

fn action_expr() -> impl Parser<char, ActionExpr, Error = Simple<char>> + Clone {
    let sequence = just('[')
        .ignore_then(ws())
        .ignore_then(
            action_call()
                .separated_by(just(',').padded_by(ws()))
                .allow_trailing(),
        )
        .then_ignore(ws())
        .then_ignore(just(']'))
        .map(ActionExpr::Sequence);

    sequence.or(action_call().map(ActionExpr::Single))
}

fn spanned_action_expr() -> impl Parser<char, Spanned<ActionExpr>, Error = Simple<char>> + Clone {
    action_expr().map_with_span(Spanned::new)
}

fn predicate() -> impl Parser<char, Spanned<Predicate>, Error = Simple<char>> + Clone {
    recursive(|pred| {
        let parens = just('(')
            .ignore_then(ws())
            .ignore_then(pred.clone())
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
                "!=" => Predicate::Ne(field, val),
                ">" => Predicate::Gt(field, val),
                "<" => Predicate::Lt(field, val),
                ">=" => Predicate::Ge(field, val),
                "<=" => Predicate::Le(field, val),
                _ => unreachable!(),
            });

        let var = spanned_ident().map(Predicate::Var);

        let atom = parens
            .or(matches.map_with_span(Spanned::new))
            .or(comparison.map_with_span(Spanned::new))
            .or(var.map_with_span(Spanned::new));

        let unary = just('!')
            .ignore_then(ws())
            .repeated()
            .then(atom)
            .foldr(|_, p| Spanned::new(Predicate::Not(Box::new(p.clone())), p.span.clone()));

        let and_chain = unary
            .clone()
            .then(
                ws().ignore_then(just("&&"))
                    .ignore_then(ws())
                    .ignore_then(unary.clone())
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
    })
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

fn non_bare_predicate() -> impl Parser<char, Spanned<Predicate>, Error = Simple<char>> + Clone {
    recursive(|pred| {
        let parens = just('(')
            .ignore_then(ws())
            .ignore_then(predicate())
            .then_ignore(ws())
            .then_ignore(just(')'));

        let matches = spanned_ident()
            .then_ignore(ws())
            .then_ignore(just('~'))
            .then_ignore(ws())
            .then(spanned_string_literal())
            .map(|(field, pattern)| Predicate::Matches(field, pattern))
            .map_with_span(Spanned::new);

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
                "!=" => Predicate::Ne(field, val),
                ">" => Predicate::Gt(field, val),
                "<" => Predicate::Lt(field, val),
                ">=" => Predicate::Ge(field, val),
                "<=" => Predicate::Le(field, val),
                _ => unreachable!(),
            })
            .map_with_span(Spanned::new);

        let var = spanned_ident()
            .map(Predicate::Var)
            .map_with_span(Spanned::new);

        let negation = just('!')
            .ignore_then(ws())
            .ignore_then(pred.clone())
            .map(|p| Predicate::Not(Box::new(p)))
            .map_with_span(Spanned::new);

        let atom = parens.or(negation).or(comparison).or(matches).or(var);

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
    })
    .try_map(|p, span| {
        if matches!(p.node, Predicate::Var(_)) {
            Err(Simple::custom(
                span,
                "bare variable not allowed as predicate block condition",
            ))
        } else {
            Ok(p)
        }
    })
}

fn predicate_block(
    stmt: impl Parser<char, Spanned<Statement>, Error = Simple<char>> + Clone,
) -> impl Parser<char, PredicateBlock, Error = Simple<char>> + Clone {
    non_bare_predicate()
        .then_ignore(ws())
        .then_ignore(just('{'))
        .then_ignore(ws())
        .then(stmt.repeated())
        .then_ignore(ws())
        .then_ignore(just('}'))
        .map(|(predicate, body)| PredicateBlock { predicate, body })
}

fn prefix_block(
    stmt: impl Parser<char, Spanned<Statement>, Error = Simple<char>> + Clone,
) -> impl Parser<char, PrefixBlock, Error = Simple<char>> + Clone {
    spanned_key_combo()
        .then_ignore(ws())
        .then_ignore(just('{'))
        .then_ignore(ws())
        .then(stmt.repeated())
        .then_ignore(ws())
        .then_ignore(just('}'))
        .map(|(key, body)| PrefixBlock { key, body })
}

fn statement() -> impl Parser<char, Spanned<Statement>, Error = Simple<char>> + Clone {
    recursive(|stmt| {
        let fn_decl_stmt = fn_decl(stmt.clone()).map(Statement::FnDecl);
        let fn_call_stmt = fn_call().map(Statement::FnCall);
        let let_binding = let_stmt().map(Statement::Let);
        let predicate_block_stmt = predicate_block(stmt.clone()).map(Statement::PredicateBlock);
        let prefix_block_stmt = prefix_block(stmt.clone()).map(Statement::PrefixBlock);
        let binding_stmt = binding().map(Statement::Binding);
        let setting_stmt = setting().map(Statement::Setting);

        fn_decl_stmt
            .or(fn_call_stmt)
            .or(let_binding)
            .or(predicate_block_stmt)
            .or(binding_stmt)
            .or(prefix_block_stmt)
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
