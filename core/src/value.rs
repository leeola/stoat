use compact_str::CompactString;
use indexmap::IndexMap;
use ordered_float::OrderedFloat;

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
    Float(OrderedFloat<f64>),
    String(CompactString),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_value_basic_functionality() {
        // Test basic SimpleValue functionality with Float variant
        let simple_float = SimpleValue::Float(OrderedFloat(std::f64::consts::E));
        assert_eq!(
            simple_float,
            SimpleValue::Float(OrderedFloat(std::f64::consts::E))
        );

        let value = SimpleValue::String(CompactString::new("test"));
        assert_eq!(value, SimpleValue::String(CompactString::new("test")));
    }

    #[test]
    fn test_value_with_floats() {
        // Test that recursive Value types work with Float variants
        let recursive_value = Value::Array(Array(vec![
            Value::String(CompactString::new("test")),
            Value::I64(42),
            Value::Float(OrderedFloat(std::f64::consts::PI)),
            Value::Map(Map(IndexMap::new())),
        ]));

        // Test equality works for the array
        let expected = Value::Array(Array(vec![
            Value::String(CompactString::new("test")),
            Value::I64(42),
            Value::Float(OrderedFloat(std::f64::consts::PI)),
            Value::Map(Map(IndexMap::new())),
        ]));
        assert_eq!(recursive_value, expected);

        // Basic equality test
        let test_value = Value::Bool(true);
        assert_eq!(test_value, Value::Bool(true));
    }

    #[test]
    fn test_ordered_float_basic_functionality() {
        // Test basic OrderedFloat functionality works
        let float_val = OrderedFloat(std::f64::consts::PI);
        assert_eq!(float_val, OrderedFloat(std::f64::consts::PI));

        // Test Value::Float creation and equality
        let value_float = Value::Float(OrderedFloat(std::f64::consts::E));
        assert_eq!(value_float, Value::Float(OrderedFloat(std::f64::consts::E)));

        // Test that we can create the Value::Float variant
        let test_cases = vec![
            Value::Float(OrderedFloat(0.0)),
            Value::Float(OrderedFloat(1.0)),
            Value::Float(OrderedFloat(-1.0)),
            Value::Float(OrderedFloat(std::f64::consts::PI)),
        ];

        for case in &test_cases {
            // Each case should equal itself
            assert_eq!(case, case);
        }
    }

    #[test]
    fn test_value_float_rkyv_serialization() {
        use rkyv::api::high::{from_bytes_unchecked, to_bytes};

        // Test Value::Float rkyv round-trip serialization
        let original = Value::Float(OrderedFloat(std::f64::consts::PI));

        // Serialize using high-level API (blazing fast!)
        let bytes =
            to_bytes::<rkyv::rancor::Error>(&original).expect("Failed to serialize Value::Float");

        // Deserialize without validation (blazing fast!)
        let deserialized: Value =
            unsafe { from_bytes_unchecked::<Value, rkyv::rancor::Error>(&bytes) }
                .expect("Failed to deserialize Value::Float");

        // Verify round-trip worked
        assert_eq!(original, deserialized);
        assert_eq!(
            deserialized,
            Value::Float(OrderedFloat(std::f64::consts::PI))
        );
    }

    #[test]
    fn test_complex_value_with_floats_rkyv_serialization() {
        use rkyv::api::high::{from_bytes_unchecked, to_bytes};

        // Test complex nested structure with floats
        let mut map_data = IndexMap::new();
        map_data.insert(
            CompactString::new("pi"),
            Value::Float(OrderedFloat(std::f64::consts::PI)),
        );
        map_data.insert(
            CompactString::new("e"),
            Value::Float(OrderedFloat(std::f64::consts::E)),
        );
        map_data.insert(
            CompactString::new("numbers"),
            Value::Array(Array(vec![
                Value::Float(OrderedFloat(1.0)),
                Value::Float(OrderedFloat(-2.5)),
                Value::I64(42),
                Value::String(CompactString::new("mixed")),
            ])),
        );

        let original = Value::Map(Map(map_data));

        // Serialize using high-level API (blazing fast!)
        let bytes =
            to_bytes::<rkyv::rancor::Error>(&original).expect("Failed to serialize complex Value");

        // Deserialize without validation (blazing fast!)
        let deserialized: Value =
            unsafe { from_bytes_unchecked::<Value, rkyv::rancor::Error>(&bytes) }
                .expect("Failed to deserialize complex Value");

        // Verify round-trip worked
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_simple_value_float_rkyv_serialization() {
        use rkyv::api::high::{from_bytes_unchecked, to_bytes};

        // Test SimpleValue::Float rkyv round-trip
        let original = SimpleValue::Float(OrderedFloat(std::f64::consts::E));

        // Serialize using high-level API (blazing fast!)
        let bytes = to_bytes::<rkyv::rancor::Error>(&original)
            .expect("Failed to serialize SimpleValue::Float");

        // Deserialize without validation (blazing fast!)
        let deserialized: SimpleValue =
            unsafe { from_bytes_unchecked::<SimpleValue, rkyv::rancor::Error>(&bytes) }
                .expect("Failed to deserialize SimpleValue::Float");

        // Verify round-trip worked
        assert_eq!(original, deserialized);
        assert_eq!(
            deserialized,
            SimpleValue::Float(OrderedFloat(std::f64::consts::E))
        );
    }
}
