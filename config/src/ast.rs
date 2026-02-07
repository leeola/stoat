use std::ops::Range;

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
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventBlock {
    pub event: EventType,
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
    And(Box<Spanned<Predicate>>, Box<Spanned<Predicate>>),
    Or(Box<Spanned<Predicate>>, Box<Spanned<Predicate>>),
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
    StateRef(String),
}
