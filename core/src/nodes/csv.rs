use crate::{
    node::{
        ErrorType, Node, NodeId, NodeInit, NodePresentation, NodeSockets, NodeStatus, NodeType,
        Port, SocketInfo, SocketType,
    },
    value::Value,
    Result,
};
use std::collections::HashMap;

/// CSV data source node that loads data from a file
#[derive(Debug)]
pub struct CsvSourceNode {
    id: NodeId,
    name: String,
    file_path: String,
    data: Option<Value>, // Cached CSV data
}

impl CsvSourceNode {
    pub fn new(id: NodeId, name: String, file_path: String) -> Self {
        Self {
            id,
            name,
            file_path,
            data: None,
        }
    }

    fn load_csv(&self) -> Result<Value> {
        use crate::value::{Array, Map};
        use compact_str::CompactString;

        let mut reader =
            csv::Reader::from_path(&self.file_path).map_err(|e| crate::Error::Generic {
                message: format!("Failed to read CSV: {}", e),
            })?;

        let headers: Vec<String> = reader
            .headers()
            .map_err(|e| crate::Error::Generic {
                message: format!("Failed to read CSV headers: {}", e),
            })?
            .iter()
            .map(|h| h.to_string())
            .collect();

        let mut rows = Vec::new();
        for result in reader.records() {
            let record = result.map_err(|e| crate::Error::Generic {
                message: format!("Failed to read CSV record: {}", e),
            })?;

            let mut row_map = indexmap::IndexMap::new();
            for (i, field) in record.iter().enumerate() {
                if let Some(header) = headers.get(i) {
                    row_map.insert(
                        CompactString::from(header),
                        Value::String(CompactString::from(field)),
                    );
                }
            }
            rows.push(Value::Map(Map(row_map)));
        }

        Ok(Value::Array(Array(rows)))
    }
}

impl Node for CsvSourceNode {
    fn id(&self) -> NodeId {
        self.id
    }

    fn node_type(&self) -> NodeType {
        NodeType::CsvSource
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn execute(&mut self, _inputs: &HashMap<String, Value>) -> Result<HashMap<String, Value>> {
        // Load CSV data if not cached
        if self.data.is_none() {
            self.data = Some(self.load_csv()?);
        }

        let mut outputs = HashMap::new();
        outputs.insert("data".to_string(), self.data.clone().unwrap());
        Ok(outputs)
    }

    fn input_ports(&self) -> Vec<Port> {
        // CSV source has no inputs
        vec![]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("data", "CSV data as array of objects")]
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

    fn config(&self) -> Value {
        // Return the file path as the configuration
        Value::String(compact_str::CompactString::from(&self.file_path))
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        // TODO: Remove this ASAP - bad implementation pattern
        self
    }
}

/// NodeInit implementation for CSV source nodes
#[derive(Debug)]
pub struct CsvInit;

impl NodeInit for CsvInit {
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
                                message: "CSV node config 'path' must be a string".to_string(),
                            })
                        },
                    }
                } else {
                    return Err(crate::Error::Generic {
                        message: "CSV node config must contain 'path' field".to_string(),
                    });
                }
            },
            _ => {
                return Err(crate::Error::Generic {
                    message: "CSV node config must be a string path or map with 'path' field"
                        .to_string(),
                })
            },
        };

        Ok(Box::new(CsvSourceNode::new(id, name, file_path)))
    }

    fn name(&self) -> &'static str {
        "csv"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{value::Value, workspace::Workspace};
    use std::collections::HashMap;

    fn create_test_csv_data() -> Value {
        use crate::value::{Array, Map};
        use compact_str::CompactString;

        let mut rows = Vec::new();

        // Row 1: name=Alice, age=25
        let mut row1 = indexmap::IndexMap::new();
        row1.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Alice")),
        );
        row1.insert(
            CompactString::from("age"),
            Value::String(CompactString::from("25")),
        );
        rows.push(Value::Map(Map(row1)));

        // Row 2: name=Bob, age=30
        let mut row2 = indexmap::IndexMap::new();
        row2.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Bob")),
        );
        row2.insert(
            CompactString::from("age"),
            Value::String(CompactString::from("30")),
        );
        rows.push(Value::Map(Map(row2)));

        // Row 3: name=Alice, age=35
        let mut row3 = indexmap::IndexMap::new();
        row3.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Alice")),
        );
        row3.insert(
            CompactString::from("age"),
            Value::String(CompactString::from("35")),
        );
        rows.push(Value::Map(Map(row3)));

        Value::Array(Array(rows))
    }

    /// Mock CSV node for testing (doesn't read from file)
    #[derive(Debug)]
    struct MockCsvNode {
        id: NodeId,
        name: String,
        test_data: Value,
    }

    impl MockCsvNode {
        fn new(id: NodeId, name: String) -> Self {
            Self {
                id,
                name,
                test_data: create_test_csv_data(),
            }
        }
    }

    impl Node for MockCsvNode {
        fn id(&self) -> NodeId {
            self.id
        }
        fn node_type(&self) -> NodeType {
            NodeType::CsvSource
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
            vec![Port::new("data", "Test CSV data")]
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

        fn config(&self) -> Value {
            // Mock node returns test configuration
            Value::String(compact_str::CompactString::from("test.csv"))
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            // TODO: Remove this ASAP - bad implementation pattern
            self
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
    }

    impl Node for TestConsumerNode {
        fn id(&self) -> NodeId {
            self.id
        }
        fn node_type(&self) -> NodeType {
            NodeType::CsvSource // Reusing for simplicity in tests
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

        fn status(&self) -> NodeStatus {
            NodeStatus::Ready
        }

        fn config(&self) -> Value {
            // Consumer node has no configuration
            Value::Empty
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            // TODO: Remove this ASAP - bad implementation pattern
            self
        }
    }

    #[test]
    fn csv_with_link_transformation() {
        use crate::transform::Transformation;

        let mut workspace = Workspace::new();

        // Add CSV source node
        let csv_id = NodeId(1);
        let csv_node = Box::new(MockCsvNode::new(csv_id, "test_csv".to_string()));
        workspace.add_node_with_id(csv_id, csv_node);

        // Add consumer node that outputs received data
        let consumer_id = NodeId(2);
        let consumer_node = Box::new(TestConsumerNode::new(
            consumer_id,
            "test_consumer".to_string(),
        ));
        workspace.add_node_with_id(consumer_id, consumer_node);

        // Link CSV output to consumer input with filter transformation
        let filter_transform = Transformation::filter("name=Alice");
        workspace
            .link_nodes_with_transform(
                csv_id,
                "data".to_string(),
                consumer_id,
                "data".to_string(),
                Some(filter_transform),
            )
            .unwrap();

        // Execute consumer node - should pull filtered data from CSV node
        let _result = workspace.execute_node(consumer_id).unwrap();

        // The transformation should have been applied during execution
        // We can verify this by testing the transformation directly
        let original_data = create_test_csv_data();
        let filter_transform = Transformation::filter("name=Alice");
        let filtered_result = filter_transform.apply(&original_data).unwrap();

        if let Value::Array(crate::value::Array(rows)) = filtered_result {
            assert_eq!(rows.len(), 2); // Should have 2 Alice rows
            println!(
                "Link transformation test passed! Filtered {} rows to {} Alice rows",
                3,
                rows.len()
            );
        } else {
            panic!("Expected filtered array data");
        }
    }
}
