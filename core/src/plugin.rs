use crate::{node::NodeId, value::Value, Result};
use std::{future::Future, path::PathBuf};

pub trait IoPlugin {
    /// Describe the data source that this impl represents.
    fn name(&self) -> &'static str;
    fn methods(&self) -> IoMethodType;
    fn init(&self) -> Box<dyn Future<Output = Box<dyn IoMethod>>>;
}

// TODO: impl DataPlugin for a generic `Box<dyn DataMethod>`. Allowing for easy closure additions of
// methods.

#[derive(Debug)]
pub struct IoMethodType {
    pub name: &'static str,
    // TODO: impl streaming support. Likely via a separate trait, `DataMethodStreaming` or
    // something, though unlike `call()` i suspect it'll require different semantics for
    // pub streaming: bool,
    // TODO: impl Schema.
    pub input: (),
    pub output: (),
}

pub trait IoMethod {
    // NIT: I might want to include a native Result-like into Value?
    fn call(&self, input: Value) -> Box<dyn Future<Output = Result<Value>>>;
}

/// Data required by plugins to save node state to disk
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeSaveData {
    /// Directory path where the node data should be saved
    pub save_dir: PathBuf,
    /// Unique identifier for this node
    pub node_id: NodeId,
    /// Node-specific configuration or state data
    pub node_data: Value,
    /// Optional metadata about the save operation
    pub metadata: Option<Value>,
}

/// Data required by plugins to load node state from disk
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeLoadData {
    /// Directory path where the node data is stored
    pub load_dir: PathBuf,
    /// Unique identifier for this node
    pub node_id: NodeId,
    /// Optional metadata about the load operation
    pub metadata: Option<Value>,
}

/// Result of a successful node save operation
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeSaveResult {
    /// Files that were created during the save
    pub files_created: Vec<PathBuf>,
    /// Any additional metadata from the save operation
    pub metadata: Option<Value>,
}

/// Result of a successful node load operation  
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeLoadResult {
    /// The loaded node data
    pub node_data: Value,
    /// Files that were read during the load
    pub files_read: Vec<PathBuf>,
    /// Any additional metadata from the load operation
    pub metadata: Option<Value>,
}

pub trait NodePersistence {
    /// Save node state to disk
    ///
    /// Plugins implement this to serialize their node state to the specified directory.
    /// The plugin is responsible for creating any necessary subdirectories and files.
    ///
    /// See also: [`NodePersistence::load_node`]
    fn save_node(
        &self,
        save_data: NodeSaveData,
    ) -> Box<dyn Future<Output = Result<NodeSaveResult>>>;

    /// Load node state from disk
    ///
    /// Plugins implement this to deserialize their node state from the specified directory.
    /// The plugin should handle missing files gracefully and return appropriate errors.
    ///
    /// See also: [`NodePersistence::save_node`]
    fn load_node(
        &self,
        load_data: NodeLoadData,
    ) -> Box<dyn Future<Output = Result<NodeLoadResult>>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{node::NodeId, value::Value};
    use std::path::PathBuf;

    #[test]
    fn node_save_data_creation() {
        let save_dir = PathBuf::from("/tmp/test");
        let node_id = NodeId(123);
        let node_data = Value::String("test_data".into());

        let save_data = NodeSaveData {
            save_dir: save_dir.clone(),
            node_id,
            node_data: node_data.clone(),
            metadata: None,
        };

        assert_eq!(save_data.save_dir, save_dir);
        assert_eq!(save_data.node_id, node_id);
        assert_eq!(save_data.node_data, node_data);
        assert!(save_data.metadata.is_none());
    }

    #[test]
    fn node_load_data_creation() {
        let load_dir = PathBuf::from("/tmp/test");
        let node_id = NodeId(456);

        let load_data = NodeLoadData {
            load_dir: load_dir.clone(),
            node_id,
            metadata: Some(Value::String("test_meta".into())),
        };

        assert_eq!(load_data.load_dir, load_dir);
        assert_eq!(load_data.node_id, node_id);
        assert!(load_data.metadata.is_some());
    }

    #[test]
    fn node_save_result_creation() {
        let files = vec![PathBuf::from("file1.txt"), PathBuf::from("file2.json")];
        let metadata = Value::String("save_meta".into());

        let result = NodeSaveResult {
            files_created: files.clone(),
            metadata: Some(metadata.clone()),
        };

        assert_eq!(result.files_created, files);
        assert_eq!(result.metadata, Some(metadata));
    }

    #[test]
    fn node_load_result_creation() {
        let node_data = Value::Array(crate::value::Array(vec![Value::I64(1), Value::I64(2)]));
        let files = vec![PathBuf::from("data.json")];

        let result = NodeLoadResult {
            node_data: node_data.clone(),
            files_read: files.clone(),
            metadata: None,
        };

        assert_eq!(result.node_data, node_data);
        assert_eq!(result.files_read, files);
        assert!(result.metadata.is_none());
    }
}
