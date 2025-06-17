use compact_str::CompactString;
use indexmap::IndexMap;
// use ordered_float::OrderedFloat;

/// A centralized data format used in Stoat for.. everything.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[rkyv(serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator, __S::Error: rkyv::rancor::Source))]
#[rkyv(deserialize_bounds(__D::Error: rkyv::rancor::Source))]
#[rkyv(bytecheck(bounds(__C: rkyv::validation::ArchiveContext, __C::Error: rkyv::rancor::Source)))]
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
    // Float(OrderedFloat<f64>),
    String(CompactString),
    Array(#[rkyv(omit_bounds)] Array),
    Map(#[rkyv(omit_bounds)] Map),
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[rkyv(serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator, __S::Error: rkyv::rancor::Source))]
#[rkyv(deserialize_bounds(__D::Error: rkyv::rancor::Source))]
#[rkyv(bytecheck(bounds(__C: rkyv::validation::ArchiveContext, __C::Error: rkyv::rancor::Source)))]
pub struct Array(#[rkyv(omit_bounds)] pub Vec<Value>);

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[rkyv(serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator, __S::Error: rkyv::rancor::Source))]
#[rkyv(deserialize_bounds(__D::Error: rkyv::rancor::Source))]
#[rkyv(bytecheck(bounds(__C: rkyv::validation::ArchiveContext, __C::Error: rkyv::rancor::Source)))]
pub struct Map(#[rkyv(omit_bounds)] pub IndexMap<CompactString, Value>);

// Basic rkyv support for simple Value variants (non-recursive)
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub enum SimpleValue {
    Empty,
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    String(CompactString),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_value_rkyv_derives() {
        // This test passes if SimpleValue compiles with rkyv derives
        let value = SimpleValue::String(CompactString::new("test"));
        assert_eq!(value, SimpleValue::String(CompactString::new("test")));

        // The presence of these type aliases proves rkyv derives worked
        type _ArchivedType = <SimpleValue as rkyv::Archive>::Archived;
        type _ResolverType = <SimpleValue as rkyv::Archive>::Resolver;
    }

    #[test]
    fn test_recursive_value_rkyv_derives() {
        // This test demonstrates that recursive Value types now compile with rkyv
        let _recursive_value = Value::Array(Array(vec![
            Value::String(CompactString::new("test")),
            Value::I64(42),
            Value::Map(Map(IndexMap::new())),
        ]));

        // These type aliases prove that recursive rkyv derives work
        type _ValueArchived = <Value as rkyv::Archive>::Archived;
        type _ArrayArchived = <Array as rkyv::Archive>::Archived;
        type _MapArchived = <Map as rkyv::Archive>::Archived;

        // Basic equality test
        let test_value = Value::Bool(true);
        assert_eq!(test_value, Value::Bool(true));
    }
}
