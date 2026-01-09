use compact_str::CompactString;
use std::collections::HashMap;

/// Dynamic argument/return value for actions.
#[derive(Debug, Clone, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    String(CompactString),
    List(Vec<Value>),
    Map(HashMap<CompactString, Value>),
}

impl Value {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Self::Map(m) => m.get(key),
            _ => None,
        }
    }
}

/// Available actions in Stoat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Exit,
    SetMode(&'static str),
}
