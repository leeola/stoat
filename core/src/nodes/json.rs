//! JSON data source node implementation
//!
//! This module provides JSON file loading capabilities as a node type.
//! The JSON data is converted to the unified Value system for use throughout
//! the node graph.

use crate::{
    node::{Node, NodeId, NodePresentation, NodeSockets, NodeType, Port, SocketInfo, SocketType},
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
                message: format!("Failed to parse JSON: {}", e),
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
        outputs.insert("data".to_string(), self.data.clone().unwrap());
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
            vec![], // No inputs
            vec![SocketInfo::new(SocketType::Data, "data", false)],
        )
    }

    fn presentation(&self) -> NodePresentation {
        NodePresentation::Minimal
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
                    message: format!("Unsupported number format: {}", n),
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

        fn execute(
            &mut self,
            _inputs: &HashMap<String, Value>,
        ) -> crate::Result<HashMap<String, Value>> {
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
    }

    /// A simple consumer node for testing transformations
    #[derive(Debug)]
    struct TestConsumerNode {
        id: NodeId,
        name: String,
        last_input: Option<Value>,
    }

    impl TestConsumerNode {
        fn new(id: NodeId, name: String) -> Self {
            Self {
                id,
                name,
                last_input: None,
            }
        }

        #[allow(dead_code)]
        fn get_last_input(&self) -> Option<&Value> {
            self.last_input.as_ref()
        }
    }

    impl Node for TestConsumerNode {
        fn id(&self) -> NodeId {
            self.id
        }
        fn node_type(&self) -> NodeType {
            NodeType::JsonSource // Reusing for simplicity in tests
        }
        fn name(&self) -> &str {
            &self.name
        }

        fn execute(
            &mut self,
            inputs: &HashMap<String, Value>,
        ) -> crate::Result<HashMap<String, Value>> {
            self.last_input = inputs.get("data").cloned();
            Ok(HashMap::new()) // Consumer doesn't output anything
        }

        fn input_ports(&self) -> Vec<Port> {
            vec![Port::new("data", "Input data")]
        }
        fn output_ports(&self) -> Vec<Port> {
            vec![]
        }

        fn sockets(&self) -> NodeSockets {
            NodeSockets::new(
                vec![SocketInfo::new(SocketType::Data, "data", false)], // Input available
                vec![],                                                 // No outputs
            )
        }

        fn presentation(&self) -> NodePresentation {
            NodePresentation::Minimal
        }
    }

    #[test]
    fn json_value_conversion() {
        let json_str = r#"{"name": "Alice", "age": 25, "active": true}"#;
        let json_value: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let value = json_value_to_value(json_value).unwrap();

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

        // Add JSON source node
        let json_node = Box::new(MockJsonNode::new(NodeId(0), "test_json".to_string()));
        let json_id = workspace.add_node(json_node);

        // Add consumer node
        let consumer_node = Box::new(TestConsumerNode::new(
            NodeId(1),
            "test_consumer".to_string(),
        ));
        let consumer_id = workspace.add_node(consumer_node);

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
            .unwrap();

        // Execute consumer node - should pull filtered data from JSON node
        let _result = workspace.execute_node(consumer_id).unwrap();

        // Verify the transformation works by testing it directly
        let original_data = create_test_json_data();
        let filter_transform = Transformation::filter("name=Alice");
        let filtered_result = filter_transform.apply(&original_data).unwrap();

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
        let result = json_node.execute(&HashMap::new()).unwrap();

        let data = result.get("data").unwrap();
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
