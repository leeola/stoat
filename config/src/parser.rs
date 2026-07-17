use crate::{
    ast::{
        Action, ActionExpr, Arg, Binding, Config, EventBlock, EventType, Expr, FnDecl, Key,
        KeyPart, LetBinding, Predicate, PredicateBlock, Setting, Spanned, Statement, ThemeBlock,
        Value,
    },
    error::ParseError,
};
use chumsky::{
    error::{Rich, RichReason},
    extra,
    prelude::*,
    span::SimpleSpan,
};

type Extra<'src> = extra::Err<Rich<'src, char>>;

fn span_to_range(span: SimpleSpan<usize>) -> std::ops::Range<usize> {
    span.into_range()
}

fn comment<'src>() -> impl Parser<'src, &'src str, (), Extra<'src>> + Clone {
    just('#')
        .then(any().and_is(just('\n').not()).repeated().count().ignored())
        .ignored()
}

fn ws<'src>() -> impl Parser<'src, &'src str, (), Extra<'src>> + Clone {
    any()
        .filter(|c: &char| c.is_whitespace())
        .ignored()
        .or(comment())
        .repeated()
        .count()
        .ignored()
}

fn required_ws<'src>() -> impl Parser<'src, &'src str, (), Extra<'src>> + Clone {
    any()
        .filter(|c: &char| c.is_whitespace())
        .repeated()
        .at_least(1)
        .count()
        .ignored()
}

fn ident<'src>() -> impl Parser<'src, &'src str, String, Extra<'src>> + Clone {
    any()
        .filter(|c: &char| c.is_ascii_alphabetic() || *c == '_')
        .then(
            any()
                .filter(|c: &char| c.is_ascii_alphanumeric() || *c == '_')
                .repeated()
                .collect::<String>(),
        )
        .map(|(first, rest): (char, String)| {
            let mut s = String::with_capacity(rest.len() + 1);
            s.push(first);
            s.push_str(&rest);
            s
        })
}

fn spanned_ident<'src>() -> impl Parser<'src, &'src str, Spanned<String>, Extra<'src>> + Clone {
    ident().map_with(|node, e| Spanned::new(node, span_to_range(e.span())))
}

fn string_literal<'src>() -> impl Parser<'src, &'src str, String, Extra<'src>> + Clone {
    just('"')
        .ignore_then(
            any()
                .filter(|c: &char| *c != '"' && *c != '\\')
                .or(just('\\').ignore_then(any()))
                .repeated()
                .collect::<String>(),
        )
        .then_ignore(just('"'))
}

fn spanned_string_literal<'src>(
) -> impl Parser<'src, &'src str, Spanned<String>, Extra<'src>> + Clone {
    string_literal().map_with(|node, e| Spanned::new(node, span_to_range(e.span())))
}

fn number<'src>() -> impl Parser<'src, &'src str, f64, Extra<'src>> + Clone {
    let digits = any()
        .filter(|c: &char| c.is_ascii_digit())
        .repeated()
        .at_least(1)
        .collect::<String>();

    let frac = just('.').then(digits).map(|(dot, ds)| {
        let mut s = String::with_capacity(ds.len() + 1);
        s.push(dot);
        s.push_str(&ds);
        s
    });

    just('-')
        .or_not()
        .then(digits)
        .then(frac.or_not())
        .map(|((sign, int_part), frac_part)| {
            let mut s = String::new();
            if let Some(c) = sign {
                s.push(c);
            }
            s.push_str(&int_part);
            if let Some(f) = frac_part {
                s.push_str(&f);
            }
            s
        })
        .try_map(|s, span: SimpleSpan<usize>| {
            s.parse::<f64>()
                .map_err(|_| Rich::custom(span, "invalid number"))
        })
}

fn enum_value<'src>() -> impl Parser<'src, &'src str, Value, Extra<'src>> + Clone {
    ident()
        .then_ignore(just("::"))
        .then(ident())
        .map(|(ty, variant)| Value::Enum { ty, variant })
}

fn value<'src>() -> impl Parser<'src, &'src str, Value, Extra<'src>> + Clone {
    recursive(|value| {
        let state_ref = just('$').ignore_then(ident()).map(Value::StateRef);

        let spanned_value = value
            .clone()
            .map_with(|node, e| Spanned::new(node, span_to_range(e.span())));

        let array = just('[')
            .ignore_then(ws())
            .ignore_then(
                spanned_value
                    .clone()
                    .separated_by(just(',').padded_by(ws()))
                    .allow_trailing()
                    .collect::<Vec<_>>(),
            )
            .then_ignore(ws())
            .then_ignore(just(']'))
            .map(Value::Array);

        let map_entry = spanned_ident()
            .then_ignore(ws())
            .then_ignore(just(':'))
            .then_ignore(ws())
            .then(spanned_value);

        let map = just('{')
            .ignore_then(ws())
            .ignore_then(
                map_entry
                    .separated_by(just(',').padded_by(ws()))
                    .allow_trailing()
                    .collect::<Vec<_>>(),
            )
            .then_ignore(ws())
            .then_ignore(just('}'))
            .map(Value::Map);

        choice((
            string_literal().map(Value::String),
            enum_value(),
            number().map(Value::Number),
            array,
            map,
            state_ref,
            ident().map(|s| match s.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => Value::Ident(s),
            }),
        ))
    })
}

fn spanned_value<'src>() -> impl Parser<'src, &'src str, Spanned<Value>, Extra<'src>> + Clone {
    value().map_with(|node, e| Spanned::new(node, span_to_range(e.span())))
}

fn expr<'src>() -> impl Parser<'src, &'src str, Expr, Extra<'src>> + Clone {
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

        let atom = choice((state_ref, string_expr, num_expr, ident_expr));

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

        let spanned_expr = expr
            .clone()
            .map_with(|node, e| Spanned::new(node, span_to_range(e.span())));
        let spanned_pred =
            predicate_inner().map_with(|node, e| Spanned::new(node, span_to_range(e.span())));

        let if_expr = just("if")
            .ignore_then(required_ws())
            .ignore_then(spanned_pred)
            .then_ignore(ws())
            .then_ignore(just("then"))
            .then_ignore(required_ws())
            .then(spanned_expr.clone())
            .then_ignore(ws())
            .then_ignore(just("else"))
            .then_ignore(required_ws())
            .then(spanned_expr)
            .map(|((cond, then_branch), else_branch)| Expr::If {
                condition: Box::new(cond),
                then_expr: Box::new(then_branch),
                else_expr: Box::new(else_branch),
            });

        if_expr.or(with_default)
    })
}

fn spanned_expr<'src>() -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone {
    expr().map_with(|node, e| Spanned::new(node, span_to_range(e.span())))
}

const KEY_TERMINATORS: &[char] = &[' ', '{', '}', '\n', '\r', '\t', '#'];

fn is_key_char(c: &char) -> bool {
    !KEY_TERMINATORS.contains(c) && *c != '-'
}

fn key<'src>() -> impl Parser<'src, &'src str, Key, Extra<'src>> + Clone {
    let named = ident().try_map(|s, span: SimpleSpan<usize>| {
        if s.len() == 1 {
            s.chars()
                .next()
                .map(Key::Char)
                .ok_or_else(|| Rich::custom(span, "empty key"))
        } else {
            Ok(Key::Named(s))
        }
    });

    let punct = any()
        .filter(|c: &char| is_key_char(c) && !c.is_ascii_alphabetic() && *c != '_')
        .map(Key::Char);

    named.or(punct)
}

fn key_part<'src>() -> impl Parser<'src, &'src str, KeyPart, Extra<'src>> + Clone {
    key()
        .separated_by(just('-'))
        .at_least(1)
        .collect::<Vec<_>>()
        .map(|keys| KeyPart { keys })
}

fn spanned_key_part<'src>() -> impl Parser<'src, &'src str, Spanned<KeyPart>, Extra<'src>> + Clone {
    key_part().map_with(|node, e| Spanned::new(node, span_to_range(e.span())))
}

fn arg<'src>() -> impl Parser<'src, &'src str, Spanned<Arg>, Extra<'src>> + Clone {
    let named = spanned_ident()
        .then_ignore(ws())
        .then_ignore(just(':'))
        .then_ignore(ws())
        .then(spanned_value())
        .map(|(name, value)| Arg::Named { name, value });

    let positional = spanned_value().map(Arg::Positional);

    named
        .or(positional)
        .map_with(|node, e| Spanned::new(node, span_to_range(e.span())))
}

fn action<'src>() -> impl Parser<'src, &'src str, Action, Extra<'src>> + Clone {
    ident()
        .then_ignore(ws())
        .then_ignore(just('('))
        .then_ignore(ws())
        .then(
            arg()
                .separated_by(just(',').padded_by(ws()))
                .allow_trailing()
                .collect::<Vec<_>>(),
        )
        .then_ignore(ws())
        .then_ignore(just(')'))
        .map(|(name, args)| Action { name, args })
}

fn spanned_action<'src>() -> impl Parser<'src, &'src str, Spanned<Action>, Extra<'src>> + Clone {
    action().map_with(|node, e| Spanned::new(node, span_to_range(e.span())))
}

fn action_expr<'src>() -> impl Parser<'src, &'src str, ActionExpr, Extra<'src>> + Clone {
    let sequence = just('[')
        .ignore_then(ws())
        .ignore_then(
            spanned_action()
                .separated_by(just(',').padded_by(ws()))
                .allow_trailing()
                .collect::<Vec<_>>(),
        )
        .then_ignore(ws())
        .then_ignore(just(']'))
        .map(ActionExpr::Sequence);

    sequence.or(action().map(ActionExpr::Single))
}

fn spanned_action_expr<'src>(
) -> impl Parser<'src, &'src str, Spanned<ActionExpr>, Extra<'src>> + Clone {
    action_expr().map_with(|node, e| Spanned::new(node, span_to_range(e.span())))
}

fn predicate_inner<'src>() -> impl Parser<'src, &'src str, Predicate, Extra<'src>> + Clone {
    recursive(|pred| {
        let spanned_pred = pred
            .clone()
            .map_with(|node, e| Spanned::new(node, span_to_range(e.span())));

        let parens = just('(')
            .ignore_then(ws())
            .ignore_then(spanned_pred)
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
            .then(choice((
                just("=="),
                just("!="),
                just(">="),
                just("<="),
                just(">"),
                just("<"),
            )))
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

        let atom = choice((
            parens,
            matches.map_with(|node, e| Spanned::new(node, span_to_range(e.span()))),
            comparison.map_with(|node, e| Spanned::new(node, span_to_range(e.span()))),
            var.map_with(|node, e| Spanned::new(node, span_to_range(e.span()))),
        ));

        let and_chain = atom.clone().foldl(
            ws().ignore_then(just("&&"))
                .ignore_then(ws())
                .ignore_then(atom.clone())
                .repeated(),
            |left, right| {
                let span = left.span.start..right.span.end;
                Spanned::new(Predicate::And(Box::new(left), Box::new(right)), span)
            },
        );

        and_chain
            .clone()
            .foldl(
                ws().ignore_then(just("||"))
                    .ignore_then(ws())
                    .ignore_then(and_chain)
                    .repeated(),
                |left, right| {
                    let span = left.span.start..right.span.end;
                    Spanned::new(Predicate::Or(Box::new(left), Box::new(right)), span)
                },
            )
            .map(|spanned| spanned.node)
    })
}

fn predicate<'src>() -> impl Parser<'src, &'src str, Spanned<Predicate>, Extra<'src>> + Clone {
    predicate_inner().map_with(|node, e| Spanned::new(node, span_to_range(e.span())))
}

fn setting<'src>() -> impl Parser<'src, &'src str, Setting, Extra<'src>> + Clone {
    spanned_ident()
        .then(
            just('.')
                .ignore_then(spanned_ident())
                .repeated()
                .collect::<Vec<_>>(),
        )
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

fn binding<'src>() -> impl Parser<'src, &'src str, Binding, Extra<'src>> + Clone {
    spanned_key_part()
        .then_ignore(ws())
        .then_ignore(just("->"))
        .then_ignore(ws())
        .then(spanned_action_expr())
        .map(|(key, action)| Binding { key, action })
}

fn let_stmt<'src>() -> impl Parser<'src, &'src str, LetBinding, Extra<'src>> + Clone {
    just("let")
        .ignore_then(required_ws())
        .ignore_then(spanned_ident())
        .then_ignore(ws())
        .then_ignore(just('='))
        .then_ignore(ws())
        .then(spanned_expr())
        .map(|(name, value)| LetBinding { name, value })
}

fn fn_decl<'src>(
    stmt: impl Parser<'src, &'src str, Spanned<Statement>, Extra<'src>> + Clone + 'src,
) -> impl Parser<'src, &'src str, FnDecl, Extra<'src>> + Clone {
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
        .then(stmt.repeated().collect::<Vec<_>>())
        .then_ignore(ws())
        .then_ignore(just('}'))
        .map(|(name, body)| FnDecl { name, body })
}

fn fn_call<'src>() -> impl Parser<'src, &'src str, Spanned<String>, Extra<'src>> + Clone {
    spanned_ident()
        .then_ignore(ws())
        .then_ignore(just('('))
        .then_ignore(ws())
        .then_ignore(just(')'))
}

fn predicate_block<'src>(
    stmt: impl Parser<'src, &'src str, Spanned<Statement>, Extra<'src>> + Clone + 'src,
) -> impl Parser<'src, &'src str, PredicateBlock, Extra<'src>> + Clone {
    predicate()
        .then_ignore(ws())
        .then_ignore(just('{'))
        .then_ignore(ws())
        .then(stmt.repeated().collect::<Vec<_>>())
        .then_ignore(ws())
        .then_ignore(just('}'))
        .map(|(predicate, body)| PredicateBlock { predicate, body })
}

fn semicolon<'src>() -> impl Parser<'src, &'src str, (), Extra<'src>> + Clone {
    ws().ignore_then(just(';')).ignored()
}

fn statement<'src>() -> impl Parser<'src, &'src str, Spanned<Statement>, Extra<'src>> + Clone {
    recursive(|stmt| {
        let fn_decl_stmt = fn_decl(stmt.clone()).map(Statement::FnDecl);
        let fn_call_stmt = fn_call().map(Statement::FnCall).then_ignore(semicolon());
        let let_binding = let_stmt().map(Statement::Let).then_ignore(semicolon());
        let predicate_block_stmt = predicate_block(stmt).map(Statement::PredicateBlock);
        let binding_stmt = binding().map(Statement::Binding).then_ignore(semicolon());
        let setting_stmt = setting().map(Statement::Setting).then_ignore(semicolon());

        choice((
            fn_decl_stmt,
            fn_call_stmt,
            let_binding,
            predicate_block_stmt,
            binding_stmt,
            setting_stmt,
        ))
        .map_with(|node, e| Spanned::new(node, span_to_range(e.span())))
        .then_ignore(ws())
    })
}

fn event_type<'src>() -> impl Parser<'src, &'src str, EventType, Extra<'src>> + Clone {
    choice((
        just("init").to(EventType::Init),
        just("buffer").to(EventType::Buffer),
        just("key").to(EventType::Key),
    ))
}

fn event_block<'src>() -> impl Parser<'src, &'src str, Spanned<EventBlock>, Extra<'src>> + Clone {
    just("on")
        .ignore_then(required_ws())
        .ignore_then(event_type())
        .then_ignore(ws())
        .then_ignore(just('{'))
        .then_ignore(ws())
        .then(statement().repeated().collect::<Vec<_>>())
        .then_ignore(ws())
        .then_ignore(just('}'))
        .map(|(event, statements)| EventBlock { event, statements })
        .map_with(|node, e| Spanned::new(node, span_to_range(e.span())))
}

fn theme_block<'src>() -> impl Parser<'src, &'src str, Spanned<ThemeBlock>, Extra<'src>> + Clone {
    just("theme")
        .ignore_then(required_ws())
        .ignore_then(spanned_ident())
        .then_ignore(ws())
        .then(
            just("inherits")
                .ignore_then(required_ws())
                .ignore_then(spanned_ident())
                .then_ignore(ws())
                .or_not(),
        )
        .then_ignore(just('{'))
        .then_ignore(ws())
        .then(statement().repeated().collect::<Vec<_>>())
        .then_ignore(ws())
        .then_ignore(just('}'))
        .map(|((name, parent), statements)| ThemeBlock {
            name,
            parent,
            statements,
        })
        .map_with(|node, e| Spanned::new(node, span_to_range(e.span())))
}

enum TopLevel {
    Event(Spanned<EventBlock>),
    Theme(Spanned<ThemeBlock>),
}

fn config<'src>() -> impl Parser<'src, &'src str, Config, Extra<'src>> {
    let item = theme_block()
        .map(TopLevel::Theme)
        .or(event_block().map(TopLevel::Event));

    ws().ignore_then(item.padded_by(ws()).repeated().collect::<Vec<_>>())
        .then_ignore(end())
        .map(|items| {
            let mut blocks = Vec::new();
            let mut themes = Vec::new();
            for item in items {
                match item {
                    TopLevel::Event(b) => blocks.push(b),
                    TopLevel::Theme(t) => themes.push(t),
                }
            }
            Config { blocks, themes }
        })
}

pub fn parser<'src>() -> impl Parser<'src, &'src str, Config, Extra<'src>> {
    config()
}

fn rich_to_parse_error(err: Rich<'_, char>) -> ParseError {
    let span = span_to_range(*err.span());
    let message = match err.reason() {
        RichReason::ExpectedFound { expected, found } => {
            let found_str = found
                .as_ref()
                .map(|c| format!("'{}'", **c))
                .unwrap_or_else(|| "end of input".to_string());
            if expected.is_empty() {
                format!("unexpected {found_str}")
            } else {
                let expected_strs: Vec<_> = expected.iter().map(|pat| format!("{pat}")).collect();
                format!(
                    "expected {}, found {}",
                    expected_strs.join(" or "),
                    found_str
                )
            }
        },
        RichReason::Custom(msg) => msg.clone(),
    };
    ParseError::new(span, message)
}

pub fn parse_action(source: &str) -> Result<Action, Vec<ParseError>> {
    let source = if source.contains('(') {
        source.to_string()
    } else {
        format!("{source}()")
    };
    let (result, errs) = action()
        .then_ignore(end())
        .parse(source.as_str())
        .into_output_errors();
    if !errs.is_empty() {
        let errors = errs.into_iter().map(rich_to_parse_error).collect();
        return Err(errors);
    }
    result.ok_or_else(|| {
        vec![ParseError::new(
            0..source.len(),
            "failed to parse action".to_string(),
        )]
    })
}

pub fn parse(source: &str) -> (Option<Config>, Vec<ParseError>) {
    let (result, errs) = parser().parse(source).into_output_errors();
    let errors = errs.into_iter().map(rich_to_parse_error).collect();
    (result, errors)
}
