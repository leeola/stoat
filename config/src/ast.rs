use std::{fmt, ops::Range};

pub type Span = Range<usize>;

#[derive(Debug, Clone, PartialEq)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub fn new(node: T, span: Span) -> Self {
        Self { node, span }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub blocks: Vec<Spanned<EventBlock>>,
    pub themes: Vec<Spanned<ThemeBlock>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventBlock {
    pub event: EventType,
    pub statements: Vec<Spanned<Statement>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThemeBlock {
    pub name: Spanned<String>,
    /// The theme named by `inherits PARENT`, whose blocks resolve before this
    /// one so this block's statements override the parent's. [`None`] for a
    /// standalone theme.
    pub parent: Option<Spanned<String>>,
    pub statements: Vec<Spanned<Statement>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    Init,
    Buffer,
    Key,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Setting(Setting),
    Binding(Binding),
    Let(LetBinding),
    FnDecl(FnDecl),
    FnCall(Spanned<String>),
    PredicateBlock(PredicateBlock),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Setting {
    pub path: Vec<Spanned<String>>,
    pub value: Spanned<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LetBinding {
    pub name: Spanned<String>,
    pub value: Spanned<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FnDecl {
    pub name: Spanned<String>,
    pub body: Vec<Spanned<Statement>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PredicateBlock {
    pub predicate: Spanned<Predicate>,
    pub body: Vec<Spanned<Statement>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    pub key: Spanned<KeyPart>,
    pub action: Spanned<ActionExpr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Named(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPart {
    pub keys: Vec<Key>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    Eq(Spanned<String>, Spanned<Value>),
    NotEq(Spanned<String>, Spanned<Value>),
    Gt(Spanned<String>, Spanned<Value>),
    Lt(Spanned<String>, Spanned<Value>),
    Gte(Spanned<String>, Spanned<Value>),
    Lte(Spanned<String>, Spanned<Value>),
    Matches(Spanned<String>, Spanned<String>),
    Bool(Spanned<String>),
    Not(Box<Spanned<Predicate>>),
    And(Box<Spanned<Predicate>>, Box<Spanned<Predicate>>),
    Or(Box<Spanned<Predicate>>, Box<Spanned<Predicate>>),
}

impl Predicate {
    /// Whether `self` and `other` state the same guard, wherever each was
    /// written.
    ///
    /// The derived [`PartialEq`] also compares source spans, so one guard
    /// written in two configs, or at two places in one, compares unequal
    /// there. Here only the fields, operators, and values count. A value
    /// written as a quoted string and as a bare identifier counts as one,
    /// since `mode == "normal"` and `mode == normal` name the same mode.
    pub fn same_guard(&self, other: &Predicate) -> bool {
        match (self, other) {
            (Predicate::Eq(a, x), Predicate::Eq(b, y))
            | (Predicate::NotEq(a, x), Predicate::NotEq(b, y))
            | (Predicate::Gt(a, x), Predicate::Gt(b, y))
            | (Predicate::Lt(a, x), Predicate::Lt(b, y))
            | (Predicate::Gte(a, x), Predicate::Gte(b, y))
            | (Predicate::Lte(a, x), Predicate::Lte(b, y)) => {
                a.node == b.node && same_value(&x.node, &y.node)
            },
            (Predicate::Matches(a, x), Predicate::Matches(b, y)) => {
                a.node == b.node && x.node == y.node
            },
            (Predicate::Bool(a), Predicate::Bool(b)) => a.node == b.node,
            (Predicate::Not(a), Predicate::Not(b)) => a.node.same_guard(&b.node),
            (Predicate::And(a, x), Predicate::And(b, y))
            | (Predicate::Or(a, x), Predicate::Or(b, y)) => {
                a.node.same_guard(&b.node) && x.node.same_guard(&y.node)
            },
            _ => false,
        }
    }
}

impl fmt::Display for Predicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Predicate::Eq(field, value) => {
                write!(f, "{} == {}", field.node, PredicateValue(&value.node))
            },
            Predicate::NotEq(field, value) => {
                write!(f, "{} != {}", field.node, PredicateValue(&value.node))
            },
            Predicate::Gt(field, value) => {
                write!(f, "{} > {}", field.node, PredicateValue(&value.node))
            },
            Predicate::Lt(field, value) => {
                write!(f, "{} < {}", field.node, PredicateValue(&value.node))
            },
            Predicate::Gte(field, value) => {
                write!(f, "{} >= {}", field.node, PredicateValue(&value.node))
            },
            Predicate::Lte(field, value) => {
                write!(f, "{} <= {}", field.node, PredicateValue(&value.node))
            },
            Predicate::Matches(field, glob) => write!(f, "{} ~ {:?}", field.node, glob.node),
            Predicate::Bool(field) => f.write_str(&field.node),
            Predicate::Not(inner) => match &inner.node {
                child @ (Predicate::And(..) | Predicate::Or(..)) => write!(f, "!({child})"),
                child => write!(f, "!{child}"),
            },
            Predicate::And(left, right) => {
                write_conjunct(f, &left.node, Conn::And)?;
                f.write_str(" && ")?;
                write_conjunct(f, &right.node, Conn::And)
            },
            Predicate::Or(left, right) => {
                write_conjunct(f, &left.node, Conn::Or)?;
                f.write_str(" || ")?;
                write_conjunct(f, &right.node, Conn::Or)
            },
        }
    }
}

/// The binary connective enclosing a child predicate, so [`write_conjunct`] can
/// decide whether the child needs parentheses.
#[derive(Clone, Copy)]
enum Conn {
    And,
    Or,
}

/// Write `child` as an operand of `parent`, parenthesizing it when it is a
/// binary predicate of the other connective.
///
/// `&&` binds tighter than `||`, so a bare `a && b || c` re-parses as
/// `(a && b) || c`. Wrapping the odd-connective child keeps the rendered source
/// re-parsing to the same tree. Same-connective children stay bare, relying on
/// the operators' associativity.
fn write_conjunct(f: &mut fmt::Formatter<'_>, child: &Predicate, parent: Conn) -> fmt::Result {
    let needs_parens = matches!(
        (parent, child),
        (Conn::And, Predicate::Or(..)) | (Conn::Or, Predicate::And(..))
    );
    if needs_parens {
        write!(f, "({child})")
    } else {
        write!(f, "{child}")
    }
}

/// Renders a scalar [`Value`] as the config source a predicate parsed it from.
///
/// Predicates only carry scalar values in practice. The compound variants
/// ([`Value::Enum`], [`Value::Array`], [`Value::Map`], [`Value::StateRef`]) fall
/// back to [`Debug`] since they never appear in a predicate.
struct PredicateValue<'a>(&'a Value);

impl fmt::Display for PredicateValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Value::Ident(name) => f.write_str(name),
            Value::String(text) => write!(f, "{text:?}"),
            Value::Number(number) => write!(f, "{number}"),
            Value::Bool(flag) => write!(f, "{flag}"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// Whether two predicate values name the same thing, for
/// [`Predicate::same_guard`].
///
/// A quoted string and a bare identifier with equal text do, because a
/// predicate compares either one against the same state string.
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::Ident(b)) | (Value::Ident(a), Value::String(b)) => a == b,
        _ => a == b,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionExpr {
    Single(Action),
    Sequence(Vec<Spanned<Action>>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub name: String,
    pub args: Vec<Spanned<Arg>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Positional(Spanned<Value>),
    Named {
        name: Spanned<String>,
        value: Spanned<Value>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Value(Value),
    If {
        condition: Box<Spanned<Predicate>>,
        then_expr: Box<Spanned<Expr>>,
        else_expr: Box<Spanned<Expr>>,
    },
    Variable(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Number(f64),
    Bool(bool),
    Ident(String),
    Enum { ty: String, variant: String },
    Array(Vec<Spanned<Value>>),
    Map(Vec<(Spanned<String>, Spanned<Value>)>),
    StateRef(String),
}

#[cfg(test)]
mod tests {
    use super::{Predicate, Statement};

    /// The guard of `on key { <text> { q -> A(); } }`, parsed after `prefix`,
    /// which shifts every span by its length.
    fn guard(prefix: &str, text: &str) -> Predicate {
        let source = format!("{prefix}on key {{ {text} {{ q -> A(); }} }}");
        let (config, errors) = crate::parse(&source);
        assert!(errors.is_empty(), "{source:?} parses, got {errors:?}");

        match &config.expect("a config").blocks[0].node.statements[0].node {
            Statement::PredicateBlock(block) => block.predicate.node.clone(),
            other => panic!("expected a predicate block, got {other:?}"),
        }
    }

    #[test]
    fn same_guard_compares_what_a_guard_says_not_where_it_sits() {
        let shifted = "# shifted\n";
        let same = |a: &str, b: &str| guard("", a).same_guard(&guard(shifted, b));

        assert_ne!(
            guard("", "mode == normal"),
            guard(shifted, "mode == normal"),
            "the derived PartialEq sees the shifted spans",
        );
        assert!(same("mode == normal", "mode == normal"), "a shifted span");
        assert!(
            same("mode == normal", r#"mode == "normal""#),
            "a string against an ident",
        );
        assert!(
            same("!modal && mode == normal", r#"!modal && mode == "normal""#),
            "through Not and And",
        );
        assert!(!same("mode == normal", "mode == insert"), "another value");
        assert!(
            !same("!modal && mode == normal", "modal && mode == normal"),
            "a Not on one side only",
        );
    }
}
