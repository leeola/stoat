//! Map node implementation for data structure transformation
//!
//! The MapNode provides powerful data transformation capabilities including:
//! - Column extraction from arrays of objects
//! - Field mapping and renaming
//! - Data reshaping (flattening, nesting)
//! - Value transformations
//! - Structure changes

use crate::{
    node::{
        Node, NodeId, NodePresentation, NodeSockets, NodeStatus, NodeType, Port, SocketInfo,
        SocketType,
    },
    value::Value,
    Result,
};
use compact_str::CompactString;
use std::collections::HashMap;

/// Operations that can be performed by the MapNode
#[derive(Debug, Clone)]
pub enum MapOperation {
    /// Extract a single column from an array of objects
    /// Example: "name" extracts just the name field from each row
    ExtractColumn { field: String },

    /// Select multiple columns, creating a new structure with only those fields
    /// Example: ["name", "age"] creates objects with only name and age
    SelectColumns { fields: Vec<String> },

    /// Rename fields in the data structure
    /// Example: {"old_name": "new_name"} renames old_name to new_name
    RenameFields { mappings: HashMap<String, String> },

    /// Add a computed field based on existing fields
    /// Example: "full_name" = "{first_name} {last_name}"
    AddComputedField {
        field_name: String,
        expression: String,
    },

    /// Flatten nested objects by bringing nested fields to the top level
    /// Example: {"user": {"name": "Alice"}} becomes {"user_name": "Alice"}
    FlattenObject { separator: String },

    /// Transform array of objects to a single object with arrays as values
    /// Example: [{"name": "A", "age": 1}, {"name": "B", "age": 2}]
    /// becomes {"name": ["A", "B"], "age": [1, 2]}
    Transpose,

    /// Apply a transformation to all values of a specific type
    /// Example: convert all strings to uppercase
    TransformValues {
        target_type: ValueType,
        transformation: ValueTransformation,
    },
}

#[derive(Debug, Clone)]
pub enum ValueType {
    String,
    Number,
    Boolean,
}

#[derive(Debug, Clone)]
pub enum ValueTransformation {
    ToUppercase,
    ToLowercase,
    ToString,
    Multiply(f64),
    Add(f64),
}

/// Map node that transforms data structures
#[derive(Debug)]
pub struct MapNode {
    id: NodeId,
    name: String,
    operation: MapOperation,
}

impl MapNode {
    pub fn new(id: NodeId, name: String, operation: MapOperation) -> Self {
        Self {
            id,
            name,
            operation,
        }
    }

    fn apply_operation(&self, data: &Value) -> Result<Value> {
        match &self.operation {
            MapOperation::ExtractColumn { field } => self.extract_column(data, field),
            MapOperation::SelectColumns { fields } => self.select_columns(data, fields),
            MapOperation::RenameFields { mappings } => self.rename_fields(data, mappings),
            MapOperation::AddComputedField {
                field_name,
                expression,
            } => self.add_computed_field(data, field_name, expression),
            MapOperation::FlattenObject { separator } => self.flatten_object(data, separator),
            MapOperation::Transpose => self.transpose(data),
            MapOperation::TransformValues {
                target_type,
                transformation,
            } => self.transform_values(data, target_type, transformation),
        }
    }

    fn extract_column(&self, data: &Value, field: &str) -> Result<Value> {
        use crate::value::{Array, Map};

        let Value::Array(Array(rows)) = data else {
            return Err(crate::Error::Generic {
                message: "ExtractColumn can only be applied to arrays".to_string(),
            });
        };

        let mut column_values = Vec::new();
        for row in rows {
            if let Value::Map(Map(map)) = row {
                if let Some(value) = map.get(field) {
                    column_values.push(value.clone());
                } else {
                    column_values.push(Value::Null);
                }
            } else {
                return Err(crate::Error::Generic {
                    message: "ExtractColumn requires array of objects".to_string(),
                });
            }
        }

        Ok(Value::Array(Array(column_values)))
    }

    fn select_columns(&self, data: &Value, fields: &[String]) -> Result<Value> {
        use crate::value::{Array, Map};

        let Value::Array(Array(rows)) = data else {
            return Err(crate::Error::Generic {
                message: "SelectColumns can only be applied to arrays".to_string(),
            });
        };

        let mut selected_rows = Vec::new();
        for row in rows {
            if let Value::Map(Map(map)) = row {
                let mut new_map = indexmap::IndexMap::new();
                for field in fields {
                    if let Some(value) = map.get(field.as_str()) {
                        new_map.insert(CompactString::from(field), value.clone());
                    }
                }
                selected_rows.push(Value::Map(Map(new_map)));
            } else {
                return Err(crate::Error::Generic {
                    message: "SelectColumns requires array of objects".to_string(),
                });
            }
        }

        Ok(Value::Array(Array(selected_rows)))
    }

    fn rename_fields(&self, data: &Value, mappings: &HashMap<String, String>) -> Result<Value> {
        use crate::value::{Array, Map};

        match data {
            Value::Array(Array(rows)) => {
                let mut renamed_rows = Vec::new();
                for row in rows {
                    if let Value::Map(Map(map)) = row {
                        let mut new_map = indexmap::IndexMap::new();
                        for (key, value) in map {
                            let new_key = mappings.get(key.as_str()).map_or(key.as_str(), |v| v);
                            new_map.insert(CompactString::from(new_key), value.clone());
                        }
                        renamed_rows.push(Value::Map(Map(new_map)));
                    } else {
                        renamed_rows.push(row.clone());
                    }
                }
                Ok(Value::Array(Array(renamed_rows)))
            },
            Value::Map(Map(map)) => {
                let mut new_map = indexmap::IndexMap::new();
                for (key, value) in map {
                    let new_key = mappings.get(key.as_str()).map_or(key.as_str(), |v| v);
                    new_map.insert(CompactString::from(new_key), value.clone());
                }
                Ok(Value::Map(Map(new_map)))
            },
            _ => Ok(data.clone()),
        }
    }

    fn add_computed_field(
        &self,
        data: &Value,
        field_name: &str,
        expression: &str,
    ) -> Result<Value> {
        use crate::value::{Array, Map};

        let Value::Array(Array(rows)) = data else {
            return Err(crate::Error::Generic {
                message: "AddComputedField can only be applied to arrays".to_string(),
            });
        };

        let mut computed_rows = Vec::new();
        for row in rows {
            if let Value::Map(Map(map)) = row {
                let mut new_map = map.clone();
                let computed_value = self.evaluate_expression(expression, map)?;
                new_map.insert(CompactString::from(field_name), computed_value);
                computed_rows.push(Value::Map(Map(new_map)));
            } else {
                computed_rows.push(row.clone());
            }
        }

        Ok(Value::Array(Array(computed_rows)))
    }

    fn flatten_object(&self, data: &Value, separator: &str) -> Result<Value> {
        use crate::value::{Array, Map};

        match data {
            Value::Array(Array(rows)) => {
                let mut flattened_rows = Vec::new();
                for row in rows {
                    if let Value::Map(Map(map)) = row {
                        let flattened_map = self.flatten_map(map, separator, "")?;
                        flattened_rows.push(Value::Map(Map(flattened_map)));
                    } else {
                        flattened_rows.push(row.clone());
                    }
                }
                Ok(Value::Array(Array(flattened_rows)))
            },
            Value::Map(Map(map)) => {
                let flattened_map = self.flatten_map(map, separator, "")?;
                Ok(Value::Map(Map(flattened_map)))
            },
            _ => Ok(data.clone()),
        }
    }

    #[allow(clippy::only_used_in_recursion)]
    fn flatten_map(
        &self,
        map: &indexmap::IndexMap<compact_str::CompactString, Value>,
        separator: &str,
        prefix: &str,
    ) -> Result<indexmap::IndexMap<compact_str::CompactString, Value>> {
        use crate::value::Map;

        let mut flattened = indexmap::IndexMap::new();

        for (key, value) in map {
            let new_key = if prefix.is_empty() {
                key.clone()
            } else {
                CompactString::from(format!("{}{}{}", prefix, separator, key))
            };

            match value {
                Value::Map(Map(nested_map)) => {
                    let nested_flattened = self.flatten_map(nested_map, separator, &new_key)?;
                    for (nested_key, nested_value) in nested_flattened {
                        flattened.insert(nested_key, nested_value);
                    }
                },
                _ => {
                    flattened.insert(new_key, value.clone());
                },
            }
        }

        Ok(flattened)
    }

    fn transpose(&self, data: &Value) -> Result<Value> {
        use crate::value::{Array, Map};

        let Value::Array(Array(rows)) = data else {
            return Err(crate::Error::Generic {
                message: "Transpose can only be applied to arrays".to_string(),
            });
        };

        if rows.is_empty() {
            return Ok(Value::Map(Map(indexmap::IndexMap::new())));
        }

        // Collect all unique field names
        let mut field_names = std::collections::HashSet::new();
        for row in rows {
            if let Value::Map(Map(map)) = row {
                for key in map.keys() {
                    field_names.insert(key.clone());
                }
            }
        }

        // Create transposed structure
        let mut transposed = indexmap::IndexMap::new();
        for field_name in field_names {
            let mut column_values = Vec::new();
            for row in rows {
                if let Value::Map(Map(map)) = row {
                    if let Some(value) = map.get(&field_name) {
                        column_values.push(value.clone());
                    } else {
                        column_values.push(Value::Null);
                    }
                }
            }
            transposed.insert(field_name, Value::Array(Array(column_values)));
        }

        Ok(Value::Map(Map(transposed)))
    }

    fn transform_values(
        &self,
        data: &Value,
        target_type: &ValueType,
        transformation: &ValueTransformation,
    ) -> Result<Value> {
        match data {
            Value::Array(arr) => {
                let mut transformed = Vec::new();
                for item in &arr.0 {
                    transformed.push(self.transform_values(item, target_type, transformation)?);
                }
                Ok(Value::Array(crate::value::Array(transformed)))
            },
            Value::Map(map) => {
                let mut transformed_map = indexmap::IndexMap::new();
                for (key, value) in &map.0 {
                    transformed_map.insert(
                        key.clone(),
                        self.transform_values(value, target_type, transformation)?,
                    );
                }
                Ok(Value::Map(crate::value::Map(transformed_map)))
            },
            _ => Ok(self.apply_value_transformation(data, target_type, transformation)),
        }
    }

    fn apply_value_transformation(
        &self,
        value: &Value,
        target_type: &ValueType,
        transformation: &ValueTransformation,
    ) -> Value {
        match (target_type, value) {
            (ValueType::String, Value::String(s)) => match transformation {
                ValueTransformation::ToUppercase => Value::String(s.to_uppercase()),
                ValueTransformation::ToLowercase => Value::String(s.to_lowercase()),
                ValueTransformation::ToString => value.clone(),
                _ => value.clone(),
            },
            (ValueType::Number, Value::I64(n)) => match transformation {
                ValueTransformation::Multiply(factor) => Value::I64((*n as f64 * factor) as i64),
                ValueTransformation::Add(addend) => Value::I64((*n as f64 + addend) as i64),
                ValueTransformation::ToString => Value::String(CompactString::from(n.to_string())),
                _ => value.clone(),
            },
            // (ValueType::Number, Value::Float(f)) => match transformation {
            //     ValueTransformation::Multiply(factor) => {
            //         Value::Float(ordered_float::OrderedFloat(f.0 * factor))
            //     },
            //     ValueTransformation::Add(addend) => {
            //         Value::Float(ordered_float::OrderedFloat(f.0 + addend))
            //     },
            //     ValueTransformation::ToString =>
            // Value::String(CompactString::from(f.to_string())),     _ => value.
            // clone(), },
            _ => value.clone(),
        }
    }

    /// Simple expression evaluator for computed fields
    /// Supports basic string interpolation like "{field1} {field2}"
    fn evaluate_expression(
        &self,
        expression: &str,
        context: &indexmap::IndexMap<compact_str::CompactString, Value>,
    ) -> Result<Value> {
        // Simple template substitution
        let mut result = expression.to_string();

        for (key, value) in context {
            let placeholder = format!("{{{}}}", key);
            if result.contains(&placeholder) {
                let value_str = match value {
                    Value::String(s) => s.as_str(),
                    Value::I64(n) => {
                        return Ok(Value::String(CompactString::from(
                            result.replace(&placeholder, &n.to_string()),
                        )))
                    },
                    Value::U64(n) => {
                        return Ok(Value::String(CompactString::from(
                            result.replace(&placeholder, &n.to_string()),
                        )))
                    },
                    // Value::Float(f) => {
                    //     return Ok(Value::String(CompactString::from(
                    //         result.replace(&placeholder, &f.to_string()),
                    //     )))
                    // },
                    Value::Bool(b) => {
                        return Ok(Value::String(CompactString::from(
                            result.replace(&placeholder, &b.to_string()),
                        )))
                    },
                    _ => "",
                };
                result = result.replace(&placeholder, value_str);
            }
        }

        Ok(Value::String(CompactString::from(result)))
    }
}

impl Node for MapNode {
    fn id(&self) -> NodeId {
        self.id
    }

    fn node_type(&self) -> NodeType {
        NodeType::Map
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn execute(&mut self, inputs: &HashMap<String, Value>) -> Result<HashMap<String, Value>> {
        let data = inputs.get("data").ok_or_else(|| crate::Error::Node {
            message: "Map node requires 'data' input".to_string(),
        })?;

        let mapped_data = self.apply_operation(data)?;

        let mut outputs = HashMap::new();
        outputs.insert("data".to_string(), mapped_data);
        Ok(outputs)
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("data", "Data to transform")]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("data", "Transformed data")]
    }

    fn sockets(&self) -> NodeSockets {
        NodeSockets::new(
            vec![SocketInfo::new(SocketType::Data, "data", true)], // Input required
            vec![SocketInfo::new(SocketType::Data, "data", false)], // Output available
        )
    }

    fn presentation(&self) -> NodePresentation {
        NodePresentation::Minimal
    }

    fn status(&self) -> NodeStatus {
        // Map nodes are always ready to execute when they have an operation configured
        NodeStatus::Ready
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        // TODO: Remove this ASAP - bad implementation pattern
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Array, Map};
    use compact_str::CompactString;
    use std::collections::HashMap;

    fn create_test_data() -> Value {
        let mut rows = Vec::new();

        // Row 1: {"name": "Alice", "age": 25, "address": {"city": "NY", "state": "NY"}}
        let mut row1 = indexmap::IndexMap::new();
        row1.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Alice")),
        );
        row1.insert(CompactString::from("age"), Value::I64(25));

        let mut address1 = indexmap::IndexMap::new();
        address1.insert(
            CompactString::from("city"),
            Value::String(CompactString::from("NY")),
        );
        address1.insert(
            CompactString::from("state"),
            Value::String(CompactString::from("NY")),
        );
        row1.insert(CompactString::from("address"), Value::Map(Map(address1)));
        rows.push(Value::Map(Map(row1)));

        // Row 2: {"name": "Bob", "age": 30, "address": {"city": "LA", "state": "CA"}}
        let mut row2 = indexmap::IndexMap::new();
        row2.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Bob")),
        );
        row2.insert(CompactString::from("age"), Value::I64(30));

        let mut address2 = indexmap::IndexMap::new();
        address2.insert(
            CompactString::from("city"),
            Value::String(CompactString::from("LA")),
        );
        address2.insert(
            CompactString::from("state"),
            Value::String(CompactString::from("CA")),
        );
        row2.insert(CompactString::from("address"), Value::Map(Map(address2)));
        rows.push(Value::Map(Map(row2)));

        Value::Array(Array(rows))
    }

    #[test]
    fn extract_column() {
        let data = create_test_data();
        let mut map_node = MapNode::new(
            NodeId(1),
            "extract_names".to_string(),
            MapOperation::ExtractColumn {
                field: "name".to_string(),
            },
        );

        let mut inputs = HashMap::new();
        inputs.insert("data".to_string(), data);

        let result = map_node.execute(&inputs).unwrap();
        let output = result.get("data").unwrap();

        if let Value::Array(Array(values)) = output {
            assert_eq!(values.len(), 2);
            assert!(matches!(values[0], Value::String(_)));
            assert!(matches!(values[1], Value::String(_)));
        } else {
            panic!("Expected array output");
        }
    }

    #[test]
    fn select_columns() {
        let data = create_test_data();
        let mut map_node = MapNode::new(
            NodeId(1),
            "select_name_age".to_string(),
            MapOperation::SelectColumns {
                fields: vec!["name".to_string(), "age".to_string()],
            },
        );

        let mut inputs = HashMap::new();
        inputs.insert("data".to_string(), data);

        let result = map_node.execute(&inputs).unwrap();
        let output = result.get("data").unwrap();

        if let Value::Array(Array(rows)) = output {
            assert_eq!(rows.len(), 2);
            if let Value::Map(Map(map)) = &rows[0] {
                assert_eq!(map.len(), 2); // Should only have name and age
                assert!(map.contains_key("name"));
                assert!(map.contains_key("age"));
                assert!(!map.contains_key("address")); // Should not have address
            } else {
                panic!("Expected map in array");
            }
        } else {
            panic!("Expected array output");
        }
    }

    #[test]
    fn rename_fields() {
        let data = create_test_data();
        let mut mappings = HashMap::new();
        mappings.insert("name".to_string(), "full_name".to_string());
        mappings.insert("age".to_string(), "years_old".to_string());

        let mut map_node = MapNode::new(
            NodeId(1),
            "rename_fields".to_string(),
            MapOperation::RenameFields { mappings },
        );

        let mut inputs = HashMap::new();
        inputs.insert("data".to_string(), data);

        let result = map_node.execute(&inputs).unwrap();
        let output = result.get("data").unwrap();

        if let Value::Array(Array(rows)) = output {
            if let Value::Map(Map(map)) = &rows[0] {
                assert!(map.contains_key("full_name"));
                assert!(map.contains_key("years_old"));
                assert!(!map.contains_key("name"));
                assert!(!map.contains_key("age"));
            } else {
                panic!("Expected map in array");
            }
        } else {
            panic!("Expected array output");
        }
    }

    #[test]
    fn flatten_object() {
        let data = create_test_data();
        let mut map_node = MapNode::new(
            NodeId(1),
            "flatten".to_string(),
            MapOperation::FlattenObject {
                separator: "_".to_string(),
            },
        );

        let mut inputs = HashMap::new();
        inputs.insert("data".to_string(), data);

        let result = map_node.execute(&inputs).unwrap();
        let output = result.get("data").unwrap();

        if let Value::Array(Array(rows)) = output {
            if let Value::Map(Map(map)) = &rows[0] {
                assert!(map.contains_key("address_city"));
                assert!(map.contains_key("address_state"));
                assert!(!map.contains_key("address")); // Nested object should be gone
            } else {
                panic!("Expected map in array");
            }
        } else {
            panic!("Expected array output");
        }
    }

    #[test]
    fn transpose() {
        let data = create_test_data();
        let mut map_node =
            MapNode::new(NodeId(1), "transpose".to_string(), MapOperation::Transpose);

        let mut inputs = HashMap::new();
        inputs.insert("data".to_string(), data);

        let result = map_node.execute(&inputs).unwrap();
        let output = result.get("data").unwrap();

        if let Value::Map(Map(transposed)) = output {
            assert!(transposed.contains_key("name"));
            assert!(transposed.contains_key("age"));

            if let Some(Value::Array(Array(names))) = transposed.get("name") {
                assert_eq!(names.len(), 2);
            } else {
                panic!("Expected name column to be an array");
            }
        } else {
            panic!("Expected map output from transpose");
        }
    }

    #[test]
    fn add_computed_field() {
        let data = create_test_data();
        let mut map_node = MapNode::new(
            NodeId(1),
            "add_computed".to_string(),
            MapOperation::AddComputedField {
                field_name: "description".to_string(),
                expression: "{name} is {age} years old".to_string(),
            },
        );

        let mut inputs = HashMap::new();
        inputs.insert("data".to_string(), data);

        let result = map_node.execute(&inputs).unwrap();
        let output = result.get("data").unwrap();

        if let Value::Array(Array(rows)) = output {
            if let Value::Map(Map(map)) = &rows[0] {
                assert!(map.contains_key("description"));
                if let Some(Value::String(desc)) = map.get("description") {
                    assert!(desc.contains("Alice"));
                    assert!(desc.contains("25"));
                } else {
                    panic!("Expected description to be a string");
                }
            } else {
                panic!("Expected map in array");
            }
        } else {
            panic!("Expected array output");
        }
    }

    #[test]
    fn transform_string_values() {
        let data = create_test_data();
        let mut map_node = MapNode::new(
            NodeId(1),
            "uppercase_strings".to_string(),
            MapOperation::TransformValues {
                target_type: ValueType::String,
                transformation: ValueTransformation::ToUppercase,
            },
        );

        let mut inputs = HashMap::new();
        inputs.insert("data".to_string(), data);

        let result = map_node.execute(&inputs).unwrap();
        let output = result.get("data").unwrap();

        if let Value::Array(Array(rows)) = output {
            if let Value::Map(Map(map)) = &rows[0] {
                if let Some(Value::String(name)) = map.get("name") {
                    assert_eq!(name.as_str(), "ALICE");
                } else {
                    panic!("Expected name to be a string");
                }
            } else {
                panic!("Expected map in array");
            }
        } else {
            panic!("Expected array output");
        }
    }
}
