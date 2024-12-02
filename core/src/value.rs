use compact_str::CompactString;
use indexmap::IndexMap;
use ordered_float::OrderedFloat;

/// A centralized data format used in Stoat for.. everything.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Value {
    /// A null-like value conceptually similar to `Undefined` vs `Null` in some languages.
    ///
    /// Where `Null` is a valid value in many languages, `Empty` will represent the true absence of
    /// a value. Hopefully with less ambiguity in handling than we see in JavaScript ;)
    Empty,
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
