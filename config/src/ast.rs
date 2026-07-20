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
