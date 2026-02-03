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
    PrefixBlock(PrefixBlock),
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
pub struct PrefixBlock {
    pub key: Spanned<KeyCombo>,
    pub body: Vec<Spanned<Statement>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    pub key: Spanned<KeyCombo>,
    pub action: Spanned<ActionExpr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyCombo {
    pub keys: Vec<KeyPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPart {
    pub modifiers: Vec<String>,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    Var(Spanned<String>),
    Eq(Spanned<String>, Spanned<Value>),
    Ne(Spanned<String>, Spanned<Value>),
    Gt(Spanned<String>, Spanned<Value>),
    Lt(Spanned<String>, Spanned<Value>),
    Ge(Spanned<String>, Spanned<Value>),
    Le(Spanned<String>, Spanned<Value>),
    Matches(Spanned<String>, Spanned<String>),
    And(Box<Spanned<Predicate>>, Box<Spanned<Predicate>>),
    Or(Box<Spanned<Predicate>>, Box<Spanned<Predicate>>),
    Not(Box<Spanned<Predicate>>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionExpr {
    Single(ActionCall),
    Sequence(Vec<ActionCall>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActionCall {
    pub name: String,
    pub args: Vec<Arg>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Positional(Expr),
    Named(String, Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Ident(String),
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    StateRef(String),
    Default(Box<Expr>, Box<Expr>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    Var(String),
    Eq(Box<Expr>, Box<Expr>),
    Ne(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Ident(String),
    Enum(String, String),
    Array(Vec<Value>),
    StateRef(String),
}
