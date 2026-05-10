use snafu::Snafu;
use stoat_config::{Predicate, Value};

/// Errors produced by [`compile_predicate`] when a
/// [`stoat_config::Predicate`] uses a comparison gpui's keymap
/// context language does not express.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum CompilePredicateError {
    #[snafu(display("predicate operator `{op}` has no gpui equivalent"))]
    UnsupportedOperator {
        op: &'static str,
        #[snafu(implicit)]
        location: snafu::Location,
    },
    #[snafu(display("predicate value kind `{kind}` cannot be expressed in gpui context strings"))]
    UnsupportedValue {
        kind: &'static str,
        #[snafu(implicit)]
        location: snafu::Location,
    },
}

/// Translate a [`stoat_config::Predicate`] into a gpui-compatible
/// keymap context predicate string suitable for
/// [`gpui::KeyBindingContextPredicate::parse`] or the `context`
/// argument of `gpui::KeyBinding::new`.
///
/// Boolean fields render as bare identifiers (`palette_open` /
/// `!palette_open`); string-valued fields render as `field == value`
/// or `field != value`. Comparison operators (`>`, `<`, `>=`, `<=`,
/// `Matches`) and value kinds beyond bool/string/ident are not
/// expressible and produce [`CompilePredicateError`].
pub fn compile_predicate(predicate: &Predicate) -> Result<String, CompilePredicateError> {
    match predicate {
        Predicate::Bool(field) => Ok(field.node.clone()),
        Predicate::Eq(field, value) => render_eq(&field.node, &value.node, false),
        Predicate::NotEq(field, value) => render_eq(&field.node, &value.node, true),
        Predicate::And(left, right) => {
            let l = compile_predicate(&left.node)?;
            let r = compile_predicate(&right.node)?;
            Ok(format!("({l} && {r})"))
        },
        Predicate::Or(left, right) => {
            let l = compile_predicate(&left.node)?;
            let r = compile_predicate(&right.node)?;
            Ok(format!("({l} || {r})"))
        },
        Predicate::Gt(..) => UnsupportedOperatorSnafu { op: ">" }.fail(),
        Predicate::Lt(..) => UnsupportedOperatorSnafu { op: "<" }.fail(),
        Predicate::Gte(..) => UnsupportedOperatorSnafu { op: ">=" }.fail(),
        Predicate::Lte(..) => UnsupportedOperatorSnafu { op: "<=" }.fail(),
        Predicate::Matches(..) => UnsupportedOperatorSnafu { op: "matches" }.fail(),
    }
}

fn render_eq(field: &str, value: &Value, negate: bool) -> Result<String, CompilePredicateError> {
    match value {
        Value::Bool(true) => Ok(if negate {
            format!("!{field}")
        } else {
            field.to_string()
        }),
        Value::Bool(false) => Ok(if negate {
            field.to_string()
        } else {
            format!("!{field}")
        }),
        Value::String(s) | Value::Ident(s) => Ok(if negate {
            format!("{field} != {s}")
        } else {
            format!("{field} == {s}")
        }),
        Value::Number(_) => UnsupportedValueSnafu { kind: "number" }.fail(),
        Value::Enum { .. } => UnsupportedValueSnafu { kind: "enum" }.fail(),
        Value::Array(_) => UnsupportedValueSnafu { kind: "array" }.fail(),
        Value::Map(_) => UnsupportedValueSnafu { kind: "map" }.fail(),
        Value::StateRef(_) => UnsupportedValueSnafu { kind: "state-ref" }.fail(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_config::Spanned;

    fn span<T>(node: T) -> Spanned<T> {
        Spanned { node, span: 0..0 }
    }

    #[test]
    fn bool_predicate() {
        let p = Predicate::Bool(span("palette_open".to_string()));
        assert_eq!(compile_predicate(&p).unwrap(), "palette_open");
    }

    #[test]
    fn eq_string() {
        let p = Predicate::Eq(
            span("mode".to_string()),
            span(Value::String("normal".to_string())),
        );
        assert_eq!(compile_predicate(&p).unwrap(), "mode == normal");
    }

    #[test]
    fn eq_bool_true_is_bare_ident() {
        let p = Predicate::Eq(span("palette_open".to_string()), span(Value::Bool(true)));
        assert_eq!(compile_predicate(&p).unwrap(), "palette_open");
    }

    #[test]
    fn eq_bool_false_is_negation() {
        let p = Predicate::Eq(span("palette_open".to_string()), span(Value::Bool(false)));
        assert_eq!(compile_predicate(&p).unwrap(), "!palette_open");
    }

    #[test]
    fn neq_string() {
        let p = Predicate::NotEq(
            span("mode".to_string()),
            span(Value::String("insert".to_string())),
        );
        assert_eq!(compile_predicate(&p).unwrap(), "mode != insert");
    }

    #[test]
    fn and_combines_with_parens() {
        let mode = Predicate::Eq(
            span("mode".to_string()),
            span(Value::String("normal".to_string())),
        );
        let palette = Predicate::Eq(span("palette_open".to_string()), span(Value::Bool(false)));
        let p = Predicate::And(Box::new(span(mode)), Box::new(span(palette)));
        assert_eq!(
            compile_predicate(&p).unwrap(),
            "(mode == normal && !palette_open)"
        );
    }

    #[test]
    fn or_combines_with_parens() {
        let a = Predicate::Bool(span("a".to_string()));
        let b = Predicate::Bool(span("b".to_string()));
        let p = Predicate::Or(Box::new(span(a)), Box::new(span(b)));
        assert_eq!(compile_predicate(&p).unwrap(), "(a || b)");
    }

    #[test]
    fn gt_unsupported() {
        let p = Predicate::Gt(span("count".to_string()), span(Value::Number(1.0)));
        assert!(matches!(
            compile_predicate(&p),
            Err(CompilePredicateError::UnsupportedOperator { op: ">", .. })
        ));
    }

    #[test]
    fn number_value_unsupported() {
        let p = Predicate::Eq(span("count".to_string()), span(Value::Number(3.0)));
        assert!(matches!(
            compile_predicate(&p),
            Err(CompilePredicateError::UnsupportedValue { kind: "number", .. })
        ));
    }
}
