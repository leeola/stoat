#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    String,
    Number,
    Bool,
}

pub struct ParamDef {
    pub name: &'static str,
    pub kind: ParamKind,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    String(String),
    Number(f64),
    Bool(bool),
}
