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
use rkyv::{Archive, Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};

/// Optimized tabular data structure for efficient storage and access
#[derive(Debug, Clone, Archive, Deserialize, Serialize, serde::Serialize, serde::Deserialize)]
#[rkyv(serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator, __S::Error: rkyv::rancor::Source))]
#[rkyv(deserialize_bounds(__D::Error: rkyv::rancor::Source))]
pub struct TableData {
    /// Column names in order
    pub columns: Vec<CompactString>,
    /// Row data stored as column-major format for better cache locality
    pub rows: Vec<TableRow>,
    /// Metadata about the table
    pub metadata: TableMetadata,
}

/// A single row in the table
#[derive(Debug, Clone, Archive, Deserialize, Serialize, serde::Serialize, serde::Deserialize)]
#[rkyv(serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator, __S::Error: rkyv::rancor::Source))]
#[rkyv(deserialize_bounds(__D::Error: rkyv::rancor::Source))]
pub struct TableRow {
    /// Cell values in column order
    pub cells: Vec<TableCell>,
}

/// Individual cell value optimized for tabular display
#[derive(Debug, Clone, Archive, Deserialize, Serialize, serde::Serialize, serde::Deserialize)]
#[rkyv(serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator, __S::Error: rkyv::rancor::Source))]
#[rkyv(deserialize_bounds(__D::Error: rkyv::rancor::Source))]
pub enum TableCell {
    Text(CompactString),
    Integer(i64),
    UnsignedInteger(u64),
    Float(f64),
    Boolean(bool),
    Empty,
}

/// Metadata about the table structure
#[derive(Debug, Clone, Archive, Deserialize, Serialize, serde::Serialize, serde::Deserialize)]
#[rkyv(serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator, __S::Error: rkyv::rancor::Source))]
#[rkyv(deserialize_bounds(__D::Error: rkyv::rancor::Source))]
pub struct TableMetadata {
    /// Total number of rows
    pub row_count: usize,
    /// Total number of columns
    pub column_count: usize,
    /// Inferred column types for optimization
    pub column_types: Vec<ColumnType>,
}

/// Column type information for display optimization
#[derive(Debug, Clone, Archive, Deserialize, Serialize, serde::Serialize, serde::Deserialize)]
#[rkyv(serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator, __S::Error: rkyv::rancor::Source))]
#[rkyv(deserialize_bounds(__D::Error: rkyv::rancor::Source))]
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

    /// Serialize to bytes using rkyv
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        rkyv::api::high::to_bytes::<rkyv::rancor::Error>(self)
            .map(|aligned_vec| aligned_vec.into_vec())
            .map_err(|e| crate::Error::Generic {
                message: format!("Failed to serialize table data: {}", e),
            })
    }

    /// Deserialize from bytes using rkyv
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        unsafe { rkyv::api::high::from_bytes_unchecked::<Self, rkyv::rancor::Error>(bytes) }
            .map_err(|e| crate::Error::Generic {
                message: format!("Failed to deserialize table data: {}", e),
            })
    }
}

/// Table viewer node that displays tabular data
pub struct TableViewerNode {
    id: NodeId,
    name: String,
    /// Cached table data in optimized format
    cached_data: Option<TableData>,
    /// Serialized cache for potential mmap usage
    cached_bytes: Option<Vec<u8>>,
    /// Cache ID for disk persistence
    cache_id: Option<u64>,
    /// Cache directory for storing table data
    cache_dir: PathBuf,
}

impl TableViewerNode {
    pub fn new(id: NodeId, name: String) -> Self {
        // Default cache directory - in real usage this would come from config
        Self::new_with_cache_dir(id, name, PathBuf::from(".stoat_cache"))
    }

    /// Create a new node with a specific cache directory
    pub fn new_with_cache_dir(id: NodeId, name: String, cache_dir: PathBuf) -> Self {
        Self {
            id,
            name,
            cached_data: None,
            cached_bytes: None,
            cache_id: None,
            cache_dir,
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

    /// Get the current cache ID
    pub fn get_cache_id(&self) -> Option<u64> {
        self.cache_id
    }

    /// Get the next available cache ID (simple counter starting at 1)
    fn get_next_cache_id() -> u64 {
        // For now, simple counter starting at 1
        // In the future this could be more sophisticated with state persistence
        1
    }

    /// Ensure the cache directory exists
    fn ensure_cache_dir(&self) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.cache_dir).map_err(|e| crate::Error::Generic {
            message: format!("Failed to create cache directory: {}", e),
        })?;
        Ok(self.cache_dir.clone())
    }

    /// Get the cache file path for a given cache ID
    fn get_cache_file_path(&self, cache_id: u64) -> Result<PathBuf> {
        let cache_dir = self.ensure_cache_dir()?;
        Ok(cache_dir.join(format!("table_{}", cache_id)))
    }

    /// Load table data from disk cache
    fn load_from_disk(&self, cache_id: u64) -> Option<TableData> {
        match self.get_cache_file_path(cache_id) {
            Ok(cache_path) => {
                if cache_path.exists() {
                    match std::fs::read(&cache_path) {
                        Ok(bytes) => match TableData::from_bytes(&bytes) {
                            Ok(data) => {
                                println!(
                                    "Cache hit: Loaded table data from {}",
                                    cache_path.display()
                                );
                                Some(data)
                            },
                            Err(e) => {
                                eprintln!(
                                    "Failed to deserialize cache file {}: {}",
                                    cache_path.display(),
                                    e
                                );
                                None
                            },
                        },
                        Err(e) => {
                            eprintln!("Failed to read cache file {}: {}", cache_path.display(), e);
                            None
                        },
                    }
                } else {
                    None
                }
            },
            Err(_) => None,
        }
    }

    /// Save table data to disk cache
    fn save_to_disk(&self, cache_id: u64, data: &TableData) -> Result<()> {
        let cache_path = self.get_cache_file_path(cache_id)?;
        let bytes = data.to_bytes()?;

        std::fs::write(&cache_path, bytes).map_err(|e| crate::Error::Generic {
            message: format!("Failed to write cache file {}: {}", cache_path.display(), e),
        })?;

        println!("Saved table data to cache: {}", cache_path.display());
        Ok(())
    }

    /// Update the cached data with new input
    fn update_cache(&mut self, input_data: &Value) -> Result<()> {
        // If we don't have a cache ID yet, assign one
        if self.cache_id.is_none() {
            self.cache_id = Some(Self::get_next_cache_id());
        }

        let cache_id = self.cache_id.unwrap();

        // Try loading from disk cache first
        if let Some(cached_data) = self.load_from_disk(cache_id) {
            // Cache hit - use existing data
            let serialized = cached_data.to_bytes()?;
            self.cached_data = Some(cached_data);
            self.cached_bytes = Some(serialized);
            return Ok(());
        }

        // Cache miss - convert from Value and save to disk
        let table_data = TableData::from_value(input_data)?;

        // Save to disk cache
        self.save_to_disk(cache_id, &table_data)?;

        // Update in-memory cache
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

        // Test rkyv serialization round trip
        let bytes = table_data.to_bytes().unwrap();
        assert!(!bytes.is_empty());

        let deserialized = TableData::from_bytes(&bytes).unwrap();

        // Verify the data is the same
        assert_eq!(
            table_data.metadata.row_count,
            deserialized.metadata.row_count
        );
        assert_eq!(
            table_data.metadata.column_count,
            deserialized.metadata.column_count
        );
        assert_eq!(table_data.columns, deserialized.columns);
        assert_eq!(table_data.rows.len(), deserialized.rows.len());

        // Check first row data
        if !table_data.rows.is_empty() && !deserialized.rows.is_empty() {
            let original_row = &table_data.rows[0];
            let deserialized_row = &deserialized.rows[0];
            assert_eq!(original_row.cells.len(), deserialized_row.cells.len());
        }
    }

    #[test]
    fn table_viewer_node_execution() {
        use tempfile::TempDir;

        // Create a temporary directory for cache
        let temp_dir = TempDir::new().unwrap();
        let cache_dir = temp_dir.path().to_path_buf();

        let mut table_node =
            TableViewerNode::new_with_cache_dir(NodeId(1), "test_table".to_string(), cache_dir);

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
        use tempfile::TempDir;

        // Create a temporary directory for cache
        let temp_dir = TempDir::new().unwrap();
        let cache_dir = temp_dir.path().to_path_buf();

        let table_node =
            TableViewerNode::new_with_cache_dir(NodeId(1), "test_table".to_string(), cache_dir);

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

    #[test]
    fn table_viewer_disk_caching() {
        use tempfile::TempDir;

        // Create a temporary directory for cache
        let temp_dir = TempDir::new().unwrap();
        let cache_dir = temp_dir.path().to_path_buf();

        let mut table_node = TableViewerNode::new_with_cache_dir(
            NodeId(1),
            "test_cache_table".to_string(),
            cache_dir.clone(),
        );

        // Initially no cache ID
        assert!(table_node.get_cache_id().is_none());

        // Execute with test data - should create cache
        let mut inputs = HashMap::new();
        inputs.insert("data".to_string(), create_test_table_data());

        let result = table_node.execute(&inputs).unwrap();
        assert!(result.is_empty()); // Table viewer is a sink

        // Should now have a cache ID
        assert!(table_node.get_cache_id().is_some());
        let cache_id = table_node.get_cache_id().unwrap();

        // Verify cache file was created
        let cache_path = cache_dir.join(format!("table_{}", cache_id));
        assert!(cache_path.exists());

        // Create a new table node with same cache ID and verify it loads from cache
        let mut table_node2 = TableViewerNode::new_with_cache_dir(
            NodeId(2),
            "test_cache_table2".to_string(),
            cache_dir,
        );

        // Manually set the cache ID to test loading
        table_node2.cache_id = Some(cache_id);

        // Execute - should load from cache
        let result2 = table_node2.execute(&inputs).unwrap();
        assert!(result2.is_empty());

        // Should have loaded the cached data
        assert!(table_node2.get_table_data().is_some());
        let cached_table = table_node2.get_table_data().unwrap();
        assert_eq!(cached_table.metadata.row_count, 2);
        assert_eq!(cached_table.metadata.column_count, 3);

        // tempdir automatically cleans up when dropped
    }

    #[test]
    fn complete_table_viewer_workflow() {
        use crate::value::{Array, Map};
        use tempfile::TempDir;

        // Create a temporary directory for cache
        let temp_dir = TempDir::new().unwrap();
        let cache_dir = temp_dir.path().to_path_buf();

        // Create test data representing CSV-like input
        let mut rows = Vec::new();
        let mut row1 = indexmap::IndexMap::new();
        row1.insert(
            CompactString::from("product"),
            Value::String(CompactString::from("Laptop")),
        );
        row1.insert(CompactString::from("price"), Value::I64(1200));
        row1.insert(CompactString::from("in_stock"), Value::Bool(true));
        rows.push(Value::Map(Map(row1)));

        let mut row2 = indexmap::IndexMap::new();
        row2.insert(
            CompactString::from("product"),
            Value::String(CompactString::from("Mouse")),
        );
        row2.insert(CompactString::from("price"), Value::I64(25));
        row2.insert(CompactString::from("in_stock"), Value::Bool(false));
        rows.push(Value::Map(Map(row2)));

        let input_data = Value::Array(Array(rows));

        // Create table viewer and process data
        let mut table_viewer = TableViewerNode::new_with_cache_dir(
            NodeId(42),
            "product_table".to_string(),
            cache_dir.clone(),
        );

        // First execution - should create cache
        let mut inputs = HashMap::new();
        inputs.insert("data".to_string(), input_data.clone());

        let result1 = table_viewer.execute(&inputs).unwrap();
        assert!(result1.is_empty()); // Sink node

        // Verify cache was created
        assert!(table_viewer.get_cache_id().is_some());
        let cache_id = table_viewer.get_cache_id().unwrap();

        let cache_file = cache_dir.join(format!("table_{}", cache_id));
        assert!(cache_file.exists());

        // Verify data structure is optimized
        let table_data = table_viewer.get_table_data().unwrap();
        assert_eq!(table_data.metadata.row_count, 2);
        assert_eq!(table_data.metadata.column_count, 3);
        assert_eq!(table_data.columns.len(), 3);

        // Verify column types were inferred
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
        assert!(has_text && has_int && has_bool);

        // Second execution - should load from cache
        let mut table_viewer2 = TableViewerNode::new_with_cache_dir(
            NodeId(43),
            "product_table2".to_string(),
            cache_dir.clone(),
        );
        table_viewer2.cache_id = Some(cache_id); // Simulate same cache ID

        let result2 = table_viewer2.execute(&inputs).unwrap();
        assert!(result2.is_empty());

        // Should have same data as first execution
        let cached_data = table_viewer2.get_table_data().unwrap();
        assert_eq!(cached_data.metadata.row_count, 2);
        assert_eq!(cached_data.metadata.column_count, 3);
    }
}
