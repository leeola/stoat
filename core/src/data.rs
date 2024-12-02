/// Types related to the unified data types within Stoat.
//
// NIT: This is a candidate for it's own crate.
use crate::value::Value;

pub enum Object {
    Value(Value),
}
