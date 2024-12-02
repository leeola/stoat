use compact_str::CompactString;
use indexmap::IndexMap;
use ordered_float::OrderedFloat;

/// A centralized data format used in Stoat for.. everything.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    Float(OrderedFloat<f64>),
    String(CompactString),
    Array(Array),
    Map(Map),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Array(pub Vec<Value>);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Map(pub IndexMap<CompactString, Value>);
