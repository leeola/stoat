use crate::{
    node::{Node, NodeId, NodeType, Port},
    value::Value,
    Result,
};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum CsvNodeType {
    CsvSource,
    Filter,
}

impl From<CsvNodeType> for NodeType {
    fn from(csv_type: CsvNodeType) -> Self {
        match csv_type {
            CsvNodeType::CsvSource => NodeType::CsvSource,
            CsvNodeType::Filter => NodeType::Filter,
        }
    }
}

/// CSV data source node that loads data from a file
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
}

/// Filter node that applies simple filtering to CSV data
pub struct FilterNode {
    id: NodeId,
    name: String,
    filter_expr: String, // Simple column=value filter for now
}

impl FilterNode {
    pub fn new(id: NodeId, name: String, filter_expr: String) -> Self {
        Self {
            id,
            name,
            filter_expr,
        }
    }

    fn apply_filter(&self, data: &Value) -> Result<Value> {
        use crate::value::{Array, Map};

        let Value::Array(Array(rows)) = data else {
            return Err(crate::Error::Node {
                message: "Filter input must be an array".to_string(),
            });
        };

        // Parse simple filter: "column=value"
        let parts: Vec<&str> = self.filter_expr.split('=').collect();
        if parts.len() != 2 {
            return Err(crate::Error::Node {
                message: format!("Invalid filter expression: {}", self.filter_expr),
            });
        }

        let column = parts[0].trim();
        let value = parts[1].trim();

        let filtered_rows: Vec<Value> = rows
            .iter()
            .filter(|row| {
                if let Value::Map(Map(map)) = row {
                    if let Some(field_value) = map.get(column) {
                        if let Value::String(s) = field_value {
                            return s.as_str() == value;
                        }
                    }
                }
                false
            })
            .cloned()
            .collect();

        Ok(Value::Array(Array(filtered_rows)))
    }
}

impl Node for FilterNode {
    fn id(&self) -> NodeId {
        self.id
    }

    fn node_type(&self) -> NodeType {
        NodeType::Filter
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn execute(&mut self, inputs: &HashMap<String, Value>) -> Result<HashMap<String, Value>> {
        let data = inputs.get("data").ok_or_else(|| crate::Error::Node {
            message: "Filter node requires 'data' input".to_string(),
        })?;

        let filtered_data = self.apply_filter(data)?;

        let mut outputs = HashMap::new();
        outputs.insert("filtered".to_string(), filtered_data);
        Ok(outputs)
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("data", "CSV data to filter")]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("filtered", "Filtered CSV data")]
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
    }

    #[test]
    fn filter_node_basic() {
        let mut filter = FilterNode::new(
            NodeId(1),
            "test_filter".to_string(),
            "name=Alice".to_string(),
        );

        let mut inputs = HashMap::new();
        inputs.insert("data".to_string(), create_test_csv_data());

        let result = filter.execute(&inputs).unwrap();
        let filtered_data = result.get("filtered").unwrap();

        if let Value::Array(crate::value::Array(rows)) = filtered_data {
            assert_eq!(rows.len(), 2); // Should have 2 Alice rows
        } else {
            panic!("Expected array output");
        }
    }

    #[test]
    fn csv_to_filter_pipeline() {
        let mut workspace = Workspace::new();

        // Add CSV source node
        let csv_node = Box::new(MockCsvNode::new(NodeId(0), "test_csv".to_string()));
        let csv_id = workspace.add_node(csv_node);

        // Add filter node
        let filter_node = Box::new(FilterNode::new(
            NodeId(0),
            "test_filter".to_string(),
            "name=Alice".to_string(),
        ));
        let filter_id = workspace.add_node(filter_node);

        // Link CSV output to filter input
        workspace
            .link_nodes(csv_id, "data".to_string(), filter_id, "data".to_string())
            .unwrap();

        // Execute filter node - should pull data from CSV node
        let result = workspace.execute_node(filter_id).unwrap();
        let filtered_data = result.get("filtered").unwrap();

        if let Value::Array(crate::value::Array(rows)) = filtered_data {
            assert_eq!(rows.len(), 2); // Should have 2 Alice rows
            println!(
                "Pipeline test passed! Filtered {} rows to {} Alice rows",
                3,
                rows.len()
            );
        } else {
            panic!("Expected array output from pipeline");
        }
    }
}
