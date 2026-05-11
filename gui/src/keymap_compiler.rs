//! Compile Stoat keymap predicates into the gpui keymap context
//! language, and document the canonical context stack that those
//! predicates target.
//!
//! # Canonical context stack
//!
//! gpui dispatches actions by walking from the focused element up
//! to the window root, collecting each ancestor's
//! [`gpui::KeyContext`] entries. A predicate matches when every
//! clause holds across the accumulated stack. Stoat pushes
//! contexts at a fixed set of entity layers; this doc block is
//! the single source of truth that the predicate language and
//! keymap authors target.
//!
//! Outer to inner, the layers are:
//!
//! 1. **Workspace** -- `gui/src/workspace.rs`. Always pushes the `"Workspace"` tag via
//!    [`crate::Workspace::build_key_context`]. Future per-workspace flags fold in as their owning
//!    features add fields to `Workspace`:
//!    - `mode == normal | insert | select | ...` -- workspace modal mode (depends on the future
//!      mode field).
//!    - `palette_open`, `finder_open` -- a modal of the named kind is open (depends on the
//!      "Foundation: ModalLayer" parent).
//!    - `claude_focused` -- the claude chat pane is focused (depends on the future claude chat
//!      entity).
//!
//! 2. **Pane** -- `gui/src/pane.rs`. Pushes the `"Pane"` tag via
//!    [`crate::Pane::build_key_context`], plus the active item's `key_context_name(cx)` when
//!    [`crate::ItemView`] returns `Some` -- e.g. `"Editor"`, `"Run"`, `"Claude"`.
//!
//! 3. **Item-specific** -- each concrete item entity pushes its own tag plus any dynamic flags,
//!    additive to the Pane layer. These items are tracked under "Foundation: Editor entity" and the
//!    corresponding feature parents:
//!    - `Editor` adds `"Editor"` plus dynamic flags `showing_completions`, `showing_hover`, and
//!      `mode == insert` while the cursor is in insert mode.
//!    - `Run` adds `"Run"`.
//!    - `ClaudeChat` adds `"Claude"`.
//!
//! 4. **Modal** -- when a modal is active, the `ModalLayer` overlay pushes a tag naming the modal
//!    type (`"FileFinder"`, `"CommandPalette"`, `"DiagnosticsPicker"`, `"Help"`, `"Rebase"`, ...).
//!    Modal contexts overlay the pane/item context rather than nesting under it: while the modal
//!    owns focus, dispatch begins inside the modal subtree and the pane/item layers are not in
//!    scope. Tracked under "Foundation: ModalLayer".
//!
//! # Predicate language
//!
//! [`compile_predicate`] translates [`stoat_config::Predicate`]
//! into the string form that gpui parses for `KeyBinding`
//! contexts. The surface supported by gpui (and therefore by
//! Stoat keymaps) is:
//!
//! - Bare identifier matches the presence of a tag pushed at any layer in the stack: `Editor`,
//!   `Pane`, `palette_open`.
//! - Key-value comparison matches when the named entry equals the given value: `mode == normal`,
//!   `mode != insert`.
//! - Boolean negation: `!palette_open`.
//! - Logical combinators: `(Pane && Editor && mode == normal)`, `(palette_open || finder_open)`.
//!
//! Numeric, enum, array, map, and state-reference values are
//! rejected with [`CompilePredicateError::UnsupportedValue`]; the
//! comparison operators `>`, `<`, `>=`, `<=`, and `matches` are
//! rejected with [`CompilePredicateError::UnsupportedOperator`].

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
