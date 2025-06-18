//! Table viewer node implementation
//!
//! This module provides a table viewer node that accepts data input and displays
//! it in a tabular format. The data is cached using rkyv for efficient serialization
//! and future mmap support.

use crate::{
    node::{Node, NodeId, NodePresentation, NodeSockets, NodeType, Port, SocketInfo, SocketType},
    value::Value,
    Result,
};
use compact_str::CompactString;
// Note: rkyv serialization will be implemented later when Value types are compatible
use std::collections::HashMap;

/// Optimized tabular data structure for efficient storage and access
#[derive(Debug, Clone)]
pub struct TableData {
    /// Column names in order
    pub columns: Vec<CompactString>,
    /// Row data stored as column-major format for better cache locality
    pub rows: Vec<TableRow>,
    /// Metadata about the table
    pub metadata: TableMetadata,
}

/// A single row in the table
#[derive(Debug, Clone)]
pub struct TableRow {
    /// Cell values in column order
    pub cells: Vec<TableCell>,
}

/// Individual cell value optimized for tabular display
#[derive(Debug, Clone)]
pub enum TableCell {
    Text(CompactString),
    Integer(i64),
    UnsignedInteger(u64),
    Float(f64),
    Boolean(bool),
    Empty,
}

/// Metadata about the table structure
#[derive(Debug, Clone)]
pub struct TableMetadata {
    /// Total number of rows
    pub row_count: usize,
    /// Total number of columns
    pub column_count: usize,
    /// Inferred column types for optimization
    pub column_types: Vec<ColumnType>,
}

/// Column type information for display optimization
#[derive(Debug, Clone)]
pub enum ColumnType {
    Text,
    Integer,
    UnsignedInteger,
    Float,
    Boolean,
    Mixed,
}

impl TableData {
    /// Convert from Value to optimized TableData
    pub fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Array(crate::value::Array(rows)) => {
                let mut table_rows = Vec::new();
                let mut columns = Vec::new();
                let mut column_types = Vec::new();

                // Extract columns from first row if it's a map
                if let Some(Value::Map(crate::value::Map(first_row))) = rows.first() {
                    columns = first_row.keys().cloned().collect();
                    column_types = vec![ColumnType::Mixed; columns.len()];
                }

                // Convert each row
                for row_value in rows {
                    if let Value::Map(crate::value::Map(row_map)) = row_value {
                        let mut cells = Vec::new();

                        for column in &columns {
                            let cell = if let Some(value) = row_map.get(column) {
                                Self::value_to_cell(value)
                            } else {
                                TableCell::Empty
                            };
                            cells.push(cell);
                        }

                        table_rows.push(TableRow { cells });
                    } else {
                        return Err(crate::Error::Generic {
                            message: "Table data must be an array of objects".to_string(),
                        });
                    }
                }

                // Infer column types
                Self::infer_column_types(&table_rows, &mut column_types);

                let row_count = table_rows.len();
                let column_count = columns.len();

                Ok(TableData {
                    columns,
                    rows: table_rows,
                    metadata: TableMetadata {
                        row_count,
                        column_count,
                        column_types,
                    },
                })
            },
            _ => Err(crate::Error::Generic {
                message: "Table viewer expects array data".to_string(),
            }),
        }
    }

    /// Convert Value to TableCell
    fn value_to_cell(value: &Value) -> TableCell {
        match value {
            Value::String(s) => TableCell::Text(s.clone()),
            Value::I64(n) => TableCell::Integer(*n),
            Value::U64(n) => TableCell::UnsignedInteger(*n),
            Value::Bool(b) => TableCell::Boolean(*b),
            Value::Empty | Value::Null => TableCell::Empty,
            // Convert other types to text representation
            _ => TableCell::Text(CompactString::from(format!("{:?}", value))),
        }
    }

    /// Infer column types from data
    fn infer_column_types(rows: &[TableRow], column_types: &mut [ColumnType]) {
        for (col_idx, col_type) in column_types.iter_mut().enumerate() {
            let mut seen_types = std::collections::HashSet::new();

            for row in rows {
                if let Some(cell) = row.cells.get(col_idx) {
                    let cell_type = match cell {
                        TableCell::Text(_) => ColumnType::Text,
                        TableCell::Integer(_) => ColumnType::Integer,
                        TableCell::UnsignedInteger(_) => ColumnType::UnsignedInteger,
                        TableCell::Float(_) => ColumnType::Float,
                        TableCell::Boolean(_) => ColumnType::Boolean,
                        TableCell::Empty => continue, // Skip empty cells for type inference
                    };
                    seen_types.insert(std::mem::discriminant(&cell_type));
                }
            }

            *col_type = if seen_types.len() <= 1 {
                // All cells are the same type (or all empty)
                rows.iter()
                    .filter_map(|row| row.cells.get(col_idx))
                    .find_map(|cell| match cell {
                        TableCell::Text(_) => Some(ColumnType::Text),
                        TableCell::Integer(_) => Some(ColumnType::Integer),
                        TableCell::UnsignedInteger(_) => Some(ColumnType::UnsignedInteger),
                        TableCell::Float(_) => Some(ColumnType::Float),
                        TableCell::Boolean(_) => Some(ColumnType::Boolean),
                        TableCell::Empty => None,
                    })
                    .unwrap_or(ColumnType::Mixed)
            } else {
                ColumnType::Mixed
            };
        }
    }

    /// Serialize to bytes using rkyv (TODO: implement when Value types support rkyv)
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        // For now, use a simple placeholder serialization
        // This will be replaced with proper rkyv serialization later
        Ok(format!(
            "TableData with {} rows, {} columns",
            self.metadata.row_count, self.metadata.column_count
        )
        .into_bytes())
    }

    /// Deserialize from bytes using rkyv (TODO: implement when Value types support rkyv)
    pub fn from_bytes(_bytes: &[u8]) -> Result<Self> {
        // Placeholder implementation
        Err(crate::Error::Generic {
            message: "TableData deserialization not yet implemented".to_string(),
        })
    }
}

/// Table viewer node that displays tabular data
pub struct TableViewerNode {
    id: NodeId,
    name: String,
    /// Cached table data in optimized format
    cached_data: Option<TableData>,
    /// Serialized cache for potential mmap usage (TODO: implement with rkyv)
    cached_bytes: Option<Vec<u8>>,
}

impl TableViewerNode {
    pub fn new(id: NodeId, name: String) -> Self {
        Self {
            id,
            name,
            cached_data: None,
            cached_bytes: None,
        }
    }

    /// Get the cached table data
    pub fn get_table_data(&self) -> Option<&TableData> {
        self.cached_data.as_ref()
    }

    /// Get the cached bytes (for mmap usage)
    pub fn get_cached_bytes(&self) -> Option<&[u8]> {
        self.cached_bytes.as_deref()
    }

    /// Update the cached data with new input
    fn update_cache(&mut self, input_data: &Value) -> Result<()> {
        let table_data = TableData::from_value(input_data)?;
        let serialized = table_data.to_bytes()?;

        self.cached_data = Some(table_data);
        self.cached_bytes = Some(serialized);

        Ok(())
    }
}

impl Node for TableViewerNode {
    fn id(&self) -> NodeId {
        self.id
    }

    fn node_type(&self) -> NodeType {
        NodeType::TableViewer
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn execute(&mut self, inputs: &HashMap<String, Value>) -> Result<HashMap<String, Value>> {
        // Update cache if we receive new data
        if let Some(input_data) = inputs.get("data") {
            self.update_cache(input_data)?;
        }

        // Table viewer is a sink - it doesn't produce output
        Ok(HashMap::new())
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("data", "Tabular data to display")]
    }

    fn output_ports(&self) -> Vec<Port> {
        // Table viewer is a sink node - no outputs
        vec![]
    }

    fn sockets(&self) -> NodeSockets {
        NodeSockets::new(
            vec![SocketInfo::new(SocketType::Data, "data", false)], /* Input available but not
                                                                     * required */
            vec![], // No outputs - this is a sink node
        )
    }

    fn presentation(&self) -> NodePresentation {
        NodePresentation::TableViewer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Array, Map};

    fn create_test_table_data() -> Value {
        let mut rows = Vec::new();

        // Row 1: {"name": "Alice", "age": 25, "active": true}
        let mut row1 = indexmap::IndexMap::new();
        row1.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Alice")),
        );
        row1.insert(CompactString::from("age"), Value::I64(25));
        row1.insert(CompactString::from("active"), Value::Bool(true));
        rows.push(Value::Map(Map(row1)));

        // Row 2: {"name": "Bob", "age": 30, "active": false}
        let mut row2 = indexmap::IndexMap::new();
        row2.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Bob")),
        );
        row2.insert(CompactString::from("age"), Value::I64(30));
        row2.insert(CompactString::from("active"), Value::Bool(false));
        rows.push(Value::Map(Map(row2)));

        Value::Array(Array(rows))
    }

    #[test]
    fn table_data_conversion() {
        let value = create_test_table_data();
        let table_data = TableData::from_value(&value).unwrap();

        assert_eq!(table_data.columns.len(), 3);
        assert_eq!(table_data.metadata.row_count, 2);
        assert_eq!(table_data.metadata.column_count, 3);

        // Check first row
        let first_row = &table_data.rows[0];
        assert_eq!(first_row.cells.len(), 3);

        // Verify data types are preserved
        if let TableCell::Text(name) = &first_row.cells[0] {
            assert!(name.contains("Alice"));
        } else {
            panic!("Expected text cell for name");
        }
    }

    #[test]
    fn table_data_serialization() {
        let value = create_test_table_data();
        let table_data = TableData::from_value(&value).unwrap();

        // Test serialization (placeholder implementation)
        let bytes = table_data.to_bytes().unwrap();
        assert!(!bytes.is_empty());

        // Test that the bytes contain expected content
        let serialized_str = String::from_utf8(bytes).unwrap();
        assert!(serialized_str.contains("2 rows"));
        assert!(serialized_str.contains("3 columns"));

        // Deserialization is not implemented yet, so test that it returns an error
        let deserialize_result = TableData::from_bytes(&[]);
        assert!(deserialize_result.is_err());
        assert!(deserialize_result
            .unwrap_err()
            .to_string()
            .contains("not yet implemented"));
    }

    #[test]
    fn table_viewer_node_execution() {
        let mut table_node = TableViewerNode::new(NodeId(1), "test_table".to_string());

        // Initially no cached data
        assert!(table_node.get_table_data().is_none());

        // Execute with test data
        let mut inputs = HashMap::new();
        inputs.insert("data".to_string(), create_test_table_data());

        let result = table_node.execute(&inputs).unwrap();

        // Table viewer doesn't produce outputs
        assert!(result.is_empty());

        // But it should have cached the data
        assert!(table_node.get_table_data().is_some());
        assert!(table_node.get_cached_bytes().is_some());

        let cached_table = table_node.get_table_data().unwrap();
        assert_eq!(cached_table.metadata.row_count, 2);
        assert_eq!(cached_table.metadata.column_count, 3);
    }

    #[test]
    fn column_type_inference() {
        let value = create_test_table_data();
        let table_data = TableData::from_value(&value).unwrap();

        // Should infer column types correctly
        assert_eq!(table_data.metadata.column_types.len(), 3);

        // Check that types were inferred (exact order depends on HashMap iteration)
        let has_text = table_data
            .metadata
            .column_types
            .iter()
            .any(|t| matches!(t, ColumnType::Text));
        let has_int = table_data
            .metadata
            .column_types
            .iter()
            .any(|t| matches!(t, ColumnType::Integer));
        let has_bool = table_data
            .metadata
            .column_types
            .iter()
            .any(|t| matches!(t, ColumnType::Boolean));

        assert!(has_text, "Should have inferred text column");
        assert!(has_int, "Should have inferred integer column");
        assert!(has_bool, "Should have inferred boolean column");
    }

    #[test]
    fn table_viewer_socket_configuration() {
        let table_node = TableViewerNode::new(NodeId(1), "test_table".to_string());

        // Test socket configuration
        let sockets = table_node.sockets();
        assert_eq!(sockets.inputs.len(), 1);
        assert_eq!(sockets.outputs.len(), 0);

        // Input should be data socket, not required
        let input_socket = &sockets.inputs[0];
        assert_eq!(input_socket.name, "data");
        assert_eq!(input_socket.socket_type, SocketType::Data);
        assert!(!input_socket.required);

        // Test presentation
        assert_eq!(table_node.presentation(), NodePresentation::TableViewer);

        // Test node type
        assert_eq!(table_node.node_type(), NodeType::TableViewer);
    }
}
