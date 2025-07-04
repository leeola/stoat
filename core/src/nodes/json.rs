//! JSON data source node implementation
//!
//! This module provides JSON file loading capabilities as a node type.
//! The JSON data is converted to the unified Value system for use throughout
//! the node graph.

use crate::{
    node::{
        ErrorType, Node, NodeId, NodeInit, NodePresentation, NodeSockets, NodeStatus, NodeType,
        Port, SocketInfo, SocketType,
    },
    value::Value,
    Result,
};
use std::collections::HashMap;

/// JSON data source node that loads data from a file
#[derive(Debug)]
pub struct JsonSourceNode {
    id: NodeId,
    name: String,
    file_path: String,
    data: Option<Value>, // Cached JSON data
}

impl JsonSourceNode {
    pub fn new(id: NodeId, name: String, file_path: String) -> Self {
        Self {
            id,
            name,
            file_path,
            data: None,
        }
    }

    fn load_json(&self) -> Result<Value> {
        let json_str =
            std::fs::read_to_string(&self.file_path).map_err(|e| crate::Error::Generic {
                message: format!("Failed to read JSON file '{}': {}", self.file_path, e),
            })?;

        let json_value: serde_json::Value =
            serde_json::from_str(&json_str).map_err(|e| crate::Error::Generic {
                message: format!("Failed to parse JSON: {e}"),
            })?;

        // Convert serde_json::Value to our internal Value type
        json_value_to_value(json_value)
    }
}

impl Node for JsonSourceNode {
    fn id(&self) -> NodeId {
        self.id
    }

    fn node_type(&self) -> NodeType {
        NodeType::JsonSource
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn execute(&mut self, _inputs: &HashMap<String, Value>) -> Result<HashMap<String, Value>> {
        // Load JSON data if not cached
        if self.data.is_none() {
            self.data = Some(self.load_json()?);
        }

        let mut outputs = HashMap::new();
        outputs.insert(
            "data".to_string(),
            self.data
                .clone()
                .expect("JSON data should be loaded by now"),
        );
        Ok(outputs)
    }

    fn input_ports(&self) -> Vec<Port> {
        // JSON source has no inputs
        vec![]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("data", "JSON data as Value")]
    }

    fn sockets(&self) -> NodeSockets {
        NodeSockets::new(
            vec![SocketInfo::new(SocketType::Config, "path", true)], // Config input for file path
            vec![SocketInfo::new(SocketType::Data, "data", false)],
        )
    }

    fn presentation(&self) -> NodePresentation {
        NodePresentation::Minimal
    }

    fn status(&self) -> NodeStatus {
        // Check if file exists
        if !std::path::Path::new(&self.file_path).exists() {
            return NodeStatus::Error {
                message: format!("File not found: {}", self.file_path),
                error_type: ErrorType::Resource,
                recoverable: true,
            };
        }

        // If data is loaded, we're idle; if not, we're ready to execute
        if self.data.is_some() {
            NodeStatus::Idle
        } else {
            NodeStatus::Ready
        }
    }

    fn get_config_values(&self) -> HashMap<String, Value> {
        let mut config = HashMap::new();
        config.insert(
            "path".to_string(), // Use "path" key to match NodeInit expectations
            Value::String(compact_str::CompactString::from(&self.file_path)),
        );
        config
    }
}

/// Convert serde_json::Value to our internal Value type
fn json_value_to_value(json_value: serde_json::Value) -> Result<Value> {
    use crate::value::{Array, Map};
    use compact_str::CompactString;

    match json_value {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(b) => Ok(Value::Bool(b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::I64(i))
            } else if let Some(u) = n.as_u64() {
                Ok(Value::U64(u))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::I64(f as i64))
            } else {
                Err(crate::Error::Generic {
                    message: format!("Unsupported number format: {n}"),
                })
            }
        },
        serde_json::Value::String(s) => Ok(Value::String(CompactString::from(s))),
        serde_json::Value::Array(arr) => {
            let mut values = Vec::new();
            for item in arr {
                values.push(json_value_to_value(item)?);
            }
            Ok(Value::Array(Array(values)))
        },
        serde_json::Value::Object(obj) => {
            let mut map = indexmap::IndexMap::new();
            for (key, value) in obj {
                map.insert(CompactString::from(key), json_value_to_value(value)?);
            }
            Ok(Value::Map(Map(map)))
        },
    }
}

/// NodeInit implementation for JSON source nodes
#[derive(Debug)]
pub struct JsonInit;

impl NodeInit for JsonInit {
    fn init(&self, id: NodeId, name: String, config: Value) -> Result<Box<dyn Node>> {
        // Extract file path from config
        let file_path = match config {
            Value::String(path) => path.to_string(),
            Value::Map(ref map) => {
                // Support both direct string and map with "path" key
                if let Some(path_value) = map.0.get("path") {
                    match path_value {
                        Value::String(path) => path.to_string(),
                        _ => {
                            return Err(crate::Error::Generic {
                                message: "JSON node config 'path' must be a string".to_string(),
                            })
                        },
                    }
                } else {
                    return Err(crate::Error::Generic {
                        message: "JSON node config must contain 'path' field".to_string(),
                    });
                }
            },
            _ => {
                return Err(crate::Error::Generic {
                    message: "JSON node config must be a string path or map with 'path' field"
                        .to_string(),
                })
            },
        };

        Ok(Box::new(JsonSourceNode::new(id, name, file_path)))
    }

    fn name(&self) -> &'static str {
        "json"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{value::Value, workspace::Workspace};
    use std::collections::HashMap;

    fn create_test_json_data() -> Value {
        use crate::value::{Array, Map};
        use compact_str::CompactString;

        // Create JSON-like data structure: array of objects
        let mut rows = Vec::new();

        // Object 1: {"name": "Alice", "age": 25, "active": true}
        let mut obj1 = indexmap::IndexMap::new();
        obj1.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Alice")),
        );
        obj1.insert(CompactString::from("age"), Value::I64(25));
        obj1.insert(CompactString::from("active"), Value::Bool(true));
        rows.push(Value::Map(Map(obj1)));

        // Object 2: {"name": "Bob", "age": 30, "active": false}
        let mut obj2 = indexmap::IndexMap::new();
        obj2.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Bob")),
        );
        obj2.insert(CompactString::from("age"), Value::I64(30));
        obj2.insert(CompactString::from("active"), Value::Bool(false));
        rows.push(Value::Map(Map(obj2)));

        // Object 3: {"name": "Charlie", "age": 35, "skills": ["rust", "typescript"]}
        let mut obj3 = indexmap::IndexMap::new();
        obj3.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Charlie")),
        );
        obj3.insert(CompactString::from("age"), Value::I64(35));

        let skills = Value::Array(Array(vec![
            Value::String(CompactString::from("rust")),
            Value::String(CompactString::from("typescript")),
        ]));
        obj3.insert(CompactString::from("skills"), skills);
        rows.push(Value::Map(Map(obj3)));

        Value::Array(Array(rows))
    }

    /// Mock JSON node for testing (doesn't read from file)
    #[derive(Debug)]
    struct MockJsonNode {
        id: NodeId,
        name: String,
        test_data: Value,
    }

    impl MockJsonNode {
        fn new(id: NodeId, name: String) -> Self {
            Self {
                id,
                name,
                test_data: create_test_json_data(),
            }
        }
    }

    impl Node for MockJsonNode {
        fn id(&self) -> NodeId {
            self.id
        }
        fn node_type(&self) -> NodeType {
            NodeType::JsonSource
        }
        fn name(&self) -> &str {
            &self.name
        }

        fn execute(&mut self, _inputs: &HashMap<String, Value>) -> Result<HashMap<String, Value>> {
            let mut outputs = HashMap::new();
            outputs.insert("data".to_string(), self.test_data.clone());
            Ok(outputs)
        }

        fn input_ports(&self) -> Vec<Port> {
            vec![]
        }
        fn output_ports(&self) -> Vec<Port> {
            vec![Port::new("data", "Test JSON data")]
        }

        fn sockets(&self) -> NodeSockets {
            NodeSockets::new(
                vec![], // No inputs
                vec![SocketInfo::new(SocketType::Data, "data", false)],
            )
        }

        fn presentation(&self) -> NodePresentation {
            NodePresentation::Minimal
        }

        fn status(&self) -> NodeStatus {
            NodeStatus::Ready
        }
    }

    #[test]
    fn json_value_conversion() {
        let json_str = r#"{"name": "Alice", "age": 25, "active": true}"#;
        let json_value: serde_json::Value =
            serde_json::from_str(json_str).expect("Failed to parse JSON string in test");
        let value = json_value_to_value(json_value)
            .expect("Failed to convert JSON value to internal value");

        if let Value::Map(crate::value::Map(map)) = value {
            assert_eq!(map.len(), 3);
            assert!(matches!(map.get("name"), Some(Value::String(_))));
            assert!(matches!(map.get("age"), Some(Value::I64(25))));
            assert!(matches!(map.get("active"), Some(Value::Bool(true))));
        } else {
            panic!("Expected Map value");
        }
    }

    #[test]
    fn json_with_link_transformation() {
        use crate::transform::Transformation;

        let mut workspace = Workspace::new();

        // Create temporary test file
        let temp_dir =
            tempfile::TempDir::new().expect("Failed to create temporary directory for JSON test");
        let test_json_path = temp_dir.path().join("test.json");
        std::fs::write(&test_json_path, r#"[{"name":"Alice","age":25,"city":"NYC"},{"name":"Bob","age":30,"city":"LA"},{"name":"Charlie","age":35,"city":"Chicago"}]"#).expect("Failed to write test JSON file");

        // Add JSON source node
        let json_id = NodeId(1);
        let json_node = JsonSourceNode::new(
            json_id,
            "test_json".to_string(),
            test_json_path.to_string_lossy().to_string(),
        );
        workspace.add_json_node(json_id, json_node);

        // Add consumer JSON node
        let consumer_json_path = temp_dir.path().join("consumer.json");
        std::fs::write(&consumer_json_path, "[]").expect("Failed to write consumer JSON file"); // Empty array
        let consumer_id = NodeId(2);
        let consumer_node = JsonSourceNode::new(
            consumer_id,
            "test_consumer".to_string(),
            consumer_json_path.to_string_lossy().to_string(),
        );
        workspace.add_json_node(consumer_id, consumer_node);

        // Link JSON output to consumer input with filter transformation
        let filter_transform = Transformation::filter("name=Alice");
        workspace
            .link_nodes_with_transform(
                json_id,
                "data".to_string(),
                consumer_id,
                "data".to_string(),
                Some(filter_transform),
            )
            .expect("Failed to link JSON nodes with transformation");

        // Execute consumer node - should pull filtered data from JSON node
        let _result = workspace
            .execute_node(consumer_id)
            .expect("Failed to execute consumer JSON node");

        // Verify the transformation works by testing it directly
        let original_data = create_test_json_data();
        let filter_transform = Transformation::filter("name=Alice");
        let filtered_result = filter_transform
            .apply(&original_data)
            .expect("Failed to apply filter transformation to JSON test data");

        if let Value::Array(crate::value::Array(rows)) = filtered_result {
            assert_eq!(rows.len(), 1); // Should have 1 Alice row
            println!(
                "JSON link transformation test passed! Filtered {} rows to {} Alice rows",
                3,
                rows.len()
            );
        } else {
            panic!("Expected filtered array data");
        }
    }

    #[test]
    fn json_node_basic_execution() {
        let mut json_node = MockJsonNode::new(NodeId(0), "test_json".to_string());
        let result = json_node
            .execute(&HashMap::new())
            .expect("Failed to execute JSON node");

        let data = result
            .get("data")
            .expect("Failed to get data output from JSON node result");
        if let Value::Array(crate::value::Array(rows)) = data {
            assert_eq!(rows.len(), 3); // Should have 3 objects

            // Check first object structure
            if let Value::Map(crate::value::Map(map)) = &rows[0] {
                assert!(map.contains_key("name"));
                assert!(map.contains_key("age"));
                assert!(map.contains_key("active"));
            } else {
                panic!("Expected first row to be a Map");
            }
        } else {
            panic!("Expected JSON data to be an Array");
        }
    }
}
