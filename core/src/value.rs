use std::collections::BTreeMap;

/// A centralized data format used in Stoat for.. everything.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum Value {
    String(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct Map<K, V = Value>(BTreeMap<K, V>);
