/// The core implementation the [`Stoat`] editor runtime. Ie the editor minus the GUI/TUI/CLI
/// interfaces.
use input::UserInput;
use std::path::{Path, PathBuf};
use view::View;
use workspace::Workspace;

pub mod config;
pub mod error;
pub mod persist {
    // TODO: save state over generic FS. Ideally configurable serialization format.
}
pub mod data;
pub mod input;
pub mod mode;
pub mod node;
pub mod nodes;
pub mod transform;
pub mod value;
pub mod view;
pub mod workspace;

pub use error::{Error, Result};

/// Configuration for initializing Stoat with state management
#[derive(Debug, Default, Clone)]
pub struct StoatConfig {
    /// Custom state directory (overrides platform default)
    pub state_dir: Option<PathBuf>,
    /// Override active workspace
    pub workspace: Option<String>,
}

/// CLI state management types - re-exported for convenience
pub mod state {
    pub use crate::workspace::{SerializableWorkspace, Workspace};
    use serde::{Deserialize, Serialize};
    use std::{
        collections::HashMap,
        fs,
        path::{Path, PathBuf},
    };

    /// CLI state management for persistent workspace configuration
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct State {
        /// Currently active workspace name
        pub active_workspace: String,
        /// Metadata for all known workspaces
        pub workspaces: HashMap<String, WorkspaceMetadata>,
        /// CLI-specific configuration
        pub config: Config,
        /// Global ID counter for nodes, workspaces, and other entities
        #[serde(default = "default_next_global_id")]
        pub next_global_id: u64,
    }

    /// Default value for next_global_id for migration from old state files
    fn default_next_global_id() -> u64 {
        1
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct WorkspaceMetadata {
        /// Display name for the workspace
        pub name: String,
        /// Optional description
        pub description: Option<String>,
        /// Path to workspace data file
        pub data_path: PathBuf,
        /// Last modified timestamp
        pub last_modified: Option<chrono::DateTime<chrono::Utc>>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Config {
        /// Default output format for commands
        pub default_output_format: String,
        /// Auto-save workspace changes
        pub auto_save: bool,
    }

    impl Default for State {
        fn default() -> Self {
            let default_workspace = "default".to_string();
            let mut workspaces = HashMap::new();

            workspaces.insert(
                default_workspace.clone(),
                WorkspaceMetadata {
                    name: default_workspace.clone(),
                    description: Some("Default workspace".to_string()),
                    data_path: default_state_dir().join("workspaces").join("default.ron"),
                    last_modified: None,
                },
            );

            Self {
                active_workspace: default_workspace,
                workspaces,
                config: Config::default(),
                next_global_id: 1,
            }
        }
    }

    impl Default for Config {
        fn default() -> Self {
            Self {
                default_output_format: "table".to_string(),
                auto_save: true,
            }
        }
    }

    impl State {
        /// Load state from the default location, creating if it doesn't exist
        pub fn load() -> Result<Self, StateError> {
            Self::load_from(&default_state_path())
        }

        /// Load state from a specific path
        pub fn load_from(path: &Path) -> Result<Self, StateError> {
            if !path.exists() {
                let state = Self::new_for_directory(path.parent().unwrap_or(Path::new(".")));
                state.save_to(path)?;
                return Ok(state);
            }

            let contents = fs::read_to_string(path).map_err(|e| StateError::Io {
                path: path.to_path_buf(),
                source: e,
            })?;

            ron::from_str(&contents).map_err(|e| StateError::Serialization { source: e.into() })
        }

        /// Create a new state instance for a specific state directory
        pub fn new_for_directory(state_dir: &Path) -> Self {
            let default_workspace = "default".to_string();
            let mut workspaces = HashMap::new();

            workspaces.insert(
                default_workspace.clone(),
                WorkspaceMetadata {
                    name: default_workspace.clone(),
                    description: Some("Default workspace".to_string()),
                    data_path: state_dir.join("workspaces").join("default.ron"),
                    last_modified: None,
                },
            );

            Self {
                active_workspace: default_workspace,
                workspaces,
                config: Config::default(),
                next_global_id: 1,
            }
        }

        /// Save state to the default location
        pub fn save(&self) -> Result<(), StateError> {
            self.save_to(&default_state_path())
        }

        /// Save state to a specific path
        pub fn save_to(&self, path: &Path) -> Result<(), StateError> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| StateError::Io {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }

            let contents = ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())
                .map_err(|e| StateError::Serialization { source: e })?;

            fs::write(path, contents).map_err(|e| StateError::Io {
                path: path.to_path_buf(),
                source: e,
            })
        }

        /// Get the current workspace metadata
        pub fn current_workspace(&self) -> Option<&WorkspaceMetadata> {
            self.workspaces.get(&self.active_workspace)
        }

        /// Set the active workspace
        pub fn set_active_workspace(&mut self, name: String) -> Result<(), StateError> {
            if !self.workspaces.contains_key(&name) {
                return Err(StateError::WorkspaceNotFound { name });
            }
            self.active_workspace = name;
            Ok(())
        }

        /// Add a new workspace
        pub fn add_workspace(
            &mut self,
            name: String,
            description: Option<String>,
        ) -> Result<(), StateError> {
            if self.workspaces.contains_key(&name) {
                return Err(StateError::WorkspaceExists { name });
            }

            // Derive state directory from existing workspace paths
            let state_dir = if let Some(existing_workspace) = self.workspaces.values().next() {
                existing_workspace
                    .data_path
                    .parent() // Remove filename
                    .and_then(|p| p.parent()) // Remove "workspaces" directory
                    .unwrap_or(&default_state_dir())
                    .to_path_buf()
            } else {
                default_state_dir()
            };

            let data_path = state_dir.join("workspaces").join(format!("{}.ron", name));
            let metadata = WorkspaceMetadata {
                name: name.clone(),
                description,
                data_path,
                last_modified: None,
            };

            self.workspaces.insert(name, metadata);
            Ok(())
        }

        /// Allocate a new globally unique ID
        pub fn allocate_id(&mut self) -> u64 {
            let id = self.next_global_id;
            self.next_global_id += 1;
            id
        }

        /// Get the next global ID without allocating it
        pub fn peek_next_id(&self) -> u64 {
            self.next_global_id
        }

        /// Get the cache directory for storing node data
        pub fn get_cache_dir(&self) -> PathBuf {
            // Extract the directory from any workspace data path, or use default
            if let Some(workspace_meta) = self.workspaces.values().next() {
                workspace_meta
                    .data_path
                    .parent()
                    .and_then(|p| p.parent()) // Go up from workspaces/ to state dir
                    .unwrap_or(&default_state_dir())
                    .join("cache")
            } else {
                default_state_dir().join("cache")
            }
        }
    }

    /// Get the default state directory
    pub fn default_state_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("stoat")
    }

    /// Get the default state file path
    pub fn default_state_path() -> PathBuf {
        default_state_dir().join("state.ron")
    }

    #[derive(Debug, thiserror::Error)]
    pub enum StateError {
        #[error("IO error at path {path}: {source}")]
        Io {
            path: PathBuf,
            #[source]
            source: std::io::Error,
        },

        #[error("Serialization error: {source}")]
        Serialization {
            #[source]
            source: ron::Error,
        },

        #[error("Workspace '{name}' not found")]
        WorkspaceNotFound { name: String },

        #[error("Workspace '{name}' already exists")]
        WorkspaceExists { name: String },
    }
}

/// The primary interface for Stoat, a canvas and node based data and text editor.
pub struct Stoat {
    /// The activate workspace taking user inputs.
    active: Workspace,
    /// Current state management
    state: state::State,
    /// State file path for persistence
    state_path: PathBuf,
}

impl Default for Stoat {
    fn default() -> Self {
        Self::new()
    }
}

impl Stoat {
    pub fn new() -> Self {
        Self::new_with_config(StoatConfig::default())
            .expect("Failed to initialize Stoat with default config")
    }

    /// Create a test Stoat instance with isolated temporary state
    /// Returns both the Stoat instance and the TempDir to keep it alive
    #[cfg(any(test, feature = "test-utils"))]
    pub fn test() -> (Self, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp directory for test");
        let config = StoatConfig {
            state_dir: Some(temp_dir.path().to_path_buf()),
            workspace: None,
        };
        let stoat = Self::new_with_config(config).expect("Failed to create test Stoat instance");
        (stoat, temp_dir)
    }

    /// Create a test Stoat instance with a specific workspace
    #[cfg(any(test, feature = "test-utils"))]
    pub fn test_with_workspace(workspace_name: impl Into<String>) -> (Self, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp directory for test");
        let mut stoat = {
            let config = StoatConfig {
                state_dir: Some(temp_dir.path().to_path_buf()),
                workspace: None,
            };
            Self::new_with_config(config).expect("Failed to create test Stoat instance")
        };

        let workspace_name = workspace_name.into();
        stoat
            .state_mut()
            .add_workspace(workspace_name.clone(), None)
            .expect("Failed to add workspace in test");
        stoat
            .state_mut()
            .set_active_workspace(workspace_name)
            .expect("Failed to set active workspace in test");

        (stoat, temp_dir)
    }

    /// Create a test Stoat instance with multiple workspaces
    #[cfg(any(test, feature = "test-utils"))]
    pub fn test_with_workspaces<S: Into<String>>(
        workspaces: impl IntoIterator<Item = (S, Option<S>)>,
        active: Option<S>,
    ) -> (Self, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp directory for test");
        let mut stoat = {
            let config = StoatConfig {
                state_dir: Some(temp_dir.path().to_path_buf()),
                workspace: None,
            };
            Self::new_with_config(config).expect("Failed to create test Stoat instance")
        };

        let mut active_workspace = None;
        for (name, description) in workspaces {
            let name = name.into();
            let description = description.map(|d| d.into());
            stoat
                .state_mut()
                .add_workspace(name.clone(), description)
                .expect("Failed to add workspace in test");
            if active_workspace.is_none() {
                active_workspace = Some(name);
            }
        }

        if let Some(active_name) = active.map(|s| s.into()).or(active_workspace) {
            stoat
                .state_mut()
                .set_active_workspace(active_name)
                .expect("Failed to set active workspace in test");
        }

        (stoat, temp_dir)
    }

    /// Create a new Stoat instance with the given configuration
    pub fn new_with_config(config: StoatConfig) -> Result<Self> {
        // Determine state path
        let state_path = config
            .state_dir
            .map(|dir| dir.join("state.ron"))
            .unwrap_or_else(state::default_state_path);

        // Load or create state
        let mut state = state::State::load_from(&state_path).map_err(|e| Error::Generic {
            message: format!("Failed to load state: {}", e),
        })?;

        // Override active workspace if specified
        if let Some(workspace_name) = &config.workspace {
            state
                .set_active_workspace(workspace_name.clone())
                .map_err(|e| Error::Generic {
                    message: format!("Failed to set workspace: {}", e),
                })?;
        }

        // Load the active workspace
        let active = if let Some(workspace_meta) = state.current_workspace() {
            Self::load_workspace_from(&workspace_meta.data_path)
                .unwrap_or_else(|_| Workspace::default())
        } else {
            Workspace::default()
        };

        Ok(Self {
            active,
            state,
            state_path,
        })
    }

    pub fn with_workspace(workspace: Workspace) -> Self {
        let state_path = state::default_state_path();
        let state = state::State::default();
        Self {
            active: workspace,
            state,
            state_path,
        }
    }

    pub fn builder() -> StoatBuilder {
        Default::default()
    }

    /// Load workspace from a Ron file
    pub fn load_workspace_from(path: &Path) -> Result<Workspace> {
        if !path.exists() {
            return Ok(Workspace::default());
        }

        let contents = std::fs::read_to_string(path).map_err(|e| Error::Io {
            message: format!("Failed to read workspace from {}: {}", path.display(), e),
        })?;

        let serializable: workspace::SerializableWorkspace =
            ron::from_str(&contents).map_err(|e| Error::Serialization {
                message: format!("Failed to deserialize workspace: {}", e),
            })?;

        // Convert back to full workspace (nodes will be empty for now)
        Ok(Workspace::from_serializable(serializable))
    }

    /// Save current workspace to a Ron file
    pub fn save_workspace_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io {
                message: format!("Failed to create directory {}: {}", parent.display(), e),
            })?;
        }

        let serializable = workspace::SerializableWorkspace::from(&self.active);
        let contents = ron::ser::to_string_pretty(&serializable, ron::ser::PrettyConfig::default())
            .map_err(|e| Error::Serialization {
                message: format!("Failed to serialize workspace: {}", e),
            })?;

        std::fs::write(path, contents).map_err(|e| Error::Io {
            message: format!("Failed to write workspace to {}: {}", path.display(), e),
        })
    }

    /// Get the current active workspace
    pub fn workspace(&self) -> &Workspace {
        &self.active
    }

    /// Get mutable access to the current active workspace
    pub fn workspace_mut(&mut self) -> &mut Workspace {
        &mut self.active
    }

    /// Save both state and workspace to persistent storage
    pub fn save(&self) -> Result<()> {
        // Save the current workspace
        if let Some(workspace_meta) = self.state.current_workspace() {
            self.save_workspace_to(&workspace_meta.data_path)?;
        }

        // Save the state
        self.state
            .save_to(&self.state_path)
            .map_err(|e| Error::Generic {
                message: format!("Failed to save state: {}", e),
            })?;

        Ok(())
    }

    /// Get read-only access to the current state
    pub fn state(&self) -> &state::State {
        &self.state
    }

    /// Get mutable access to the current state
    pub fn state_mut(&mut self) -> &mut state::State {
        &mut self.state
    }

    /// Create a node using the registry with proper configuration
    pub fn create_node(
        &mut self,
        node_type: &str,
        name: String,
        config: crate::value::Value,
    ) -> crate::Result<crate::node::NodeId> {
        let id = crate::node::NodeId(self.state.allocate_id());

        // For table nodes, add cache configuration
        let final_config = if node_type == "table" {
            let cache_id = self.state.allocate_id();
            let cache_dir = self.state.get_cache_dir();

            let mut config_map = indexmap::IndexMap::new();
            config_map.insert(
                compact_str::CompactString::from("cache_id"),
                crate::value::Value::U64(cache_id),
            );
            config_map.insert(
                compact_str::CompactString::from("cache_dir"),
                crate::value::Value::String(compact_str::CompactString::from(
                    cache_dir.to_string_lossy().as_ref(),
                )),
            );

            // Merge with any existing config
            match config {
                crate::value::Value::Map(ref existing_map) => {
                    // Add existing config to our config
                    for (key, value) in &existing_map.0 {
                        config_map.insert(key.clone(), value.clone());
                    }
                },
                crate::value::Value::Empty | crate::value::Value::Null => {
                    // Just use our config
                },
                _ => {
                    // Non-map config, just use our config
                },
            }

            crate::value::Value::Map(crate::value::Map(config_map))
        } else {
            config
        };

        let node = crate::node::create_node_from_registry(node_type, id, name, final_config)?;
        self.active.add_node_with_id(id, node);
        Ok(id)
    }

    /// Add a node to the active workspace with a globally unique ID
    ///
    /// Note: For new code, prefer using `create_node` which handles proper configuration
    /// including cache setup for table nodes.
    pub fn add_node(&mut self, node: Box<dyn crate::node::Node>) -> crate::node::NodeId {
        let id = crate::node::NodeId(self.state.allocate_id());
        self.active.add_node_with_id(id, node);
        id
    }
    /// Push a user input into Stoat.
    //
    // TODO: UserInput needs to return available actions? For automatic ? and client validation?
    pub fn user_input(&mut self, _ue: impl Into<UserInput>) {
        todo!()
    }
    /// Push multiple user events into Stoat.
    pub fn user_inputs<T: Into<UserInput>>(&mut self, user_inputs: impl IntoIterator<Item = T>) {
        for ue in user_inputs {
            self.user_input(ue)
        }
    }
    pub fn view(&self) -> &View {
        self.active.view()
    }
}

#[derive(Default)]
pub struct StoatBuilder(Stoat);
impl StoatBuilder {
    /// Include standard configuration and node types.
    pub fn std(self) -> Self {
        self
    }
    pub fn build(self) -> Stoat {
        self.0
    }
}

// Workspace -> Node <->  Link

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_stoat_new_with_default_config() {
        let (stoat, _temp_dir) = Stoat::test();
        assert_eq!(stoat.state().active_workspace, "default");
    }

    #[test]
    fn test_stoat_config_with_custom_workspace() {
        let (stoat, _temp_dir) = Stoat::test_with_workspace("test");
        assert_eq!(stoat.state().active_workspace, "test");
    }

    #[test]
    fn test_stoat_save_and_load_state() {
        let temp_dir = TempDir::new().unwrap();
        let state_dir = temp_dir.path().to_path_buf();

        // Create first instance and save state
        let stoat1 = {
            let config = StoatConfig {
                state_dir: Some(state_dir.clone()),
                workspace: None,
            };
            Stoat::new_with_config(config).unwrap()
        };
        stoat1.save().unwrap();

        // Create second instance that loads the saved state
        let stoat2 = {
            let config = StoatConfig {
                state_dir: Some(state_dir),
                workspace: None,
            };
            Stoat::new_with_config(config).unwrap()
        };

        assert_eq!(
            stoat1.state().active_workspace,
            stoat2.state().active_workspace
        );
    }

    #[test]
    fn test_stoat_invalid_workspace_fails() {
        // This should fail because we're trying to switch to a non-existent workspace
        let config = StoatConfig {
            state_dir: Some(TempDir::new().unwrap().path().to_path_buf()),
            workspace: Some("nonexistent".to_string()),
        };
        let result = Stoat::new_with_config(config);
        assert!(result.is_err());
    }

    #[test]
    fn test_stoat_multiple_workspaces() {
        let workspaces = [
            ("dev", Some("Development workspace")),
            ("prod", Some("Production workspace")),
        ];
        let (stoat, _temp_dir) = Stoat::test_with_workspaces(workspaces, Some("dev"));

        assert_eq!(stoat.state().active_workspace, "dev");
        assert!(stoat.state().workspaces.contains_key("dev"));
        assert!(stoat.state().workspaces.contains_key("prod"));
        assert_eq!(stoat.state().workspaces.len(), 3); // default + dev + prod
    }

    #[test]
    fn test_stoat_custom_state_dir() {
        // This test uses the new_with_config directly since we need custom state dir
        let temp_dir = TempDir::new().unwrap();
        let custom_state_dir = temp_dir.path().join("custom/path");
        let config = StoatConfig {
            state_dir: Some(custom_state_dir),
            workspace: None,
        };
        let stoat = Stoat::new_with_config(config).unwrap();

        assert_eq!(stoat.state().active_workspace, "default");
        // State should be saved in the custom directory
        stoat.save().unwrap();
    }

    #[test]
    fn demo_old_vs_new_test_style() {
        // OLD STYLE (8+ lines of boilerplate):
        // let temp_dir = TempDir::new().unwrap();
        // let config = StoatConfig {
        //     state_dir: Some(temp_dir.path().to_path_buf()),
        //     workspace: None,
        // };
        // let mut stoat = Stoat::new_with_config(config).unwrap();
        // stoat.state_mut().add_workspace("test".to_string(), None).unwrap();
        // stoat.state_mut().set_active_workspace("test".to_string()).unwrap();

        // NEW STYLE (1 line):
        let (stoat, _temp_dir) = Stoat::test_with_workspace("test");

        assert_eq!(stoat.state().active_workspace, "test");
        assert!(stoat.state().workspaces.contains_key("test"));
    }

    #[test]
    fn test_global_id_uniqueness_across_workspaces() {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let state_dir = temp_dir.path().to_path_buf();

        // Create first workspace and add nodes
        let mut stoat1 = {
            let config = StoatConfig {
                state_dir: Some(state_dir.clone()),
                workspace: None,
            };
            Stoat::new_with_config(config).unwrap()
        };

        // Add a node in default workspace
        let node1 = Box::new(crate::nodes::table::TableViewerNode::new(
            crate::node::NodeId(0), // This will be replaced
            "table1".to_string(),
        ));
        let id1 = stoat1.add_node(node1);

        // Create second workspace
        stoat1
            .state_mut()
            .add_workspace("workspace2".to_string(), None)
            .unwrap();
        stoat1
            .state_mut()
            .set_active_workspace("workspace2".to_string())
            .unwrap();

        // Add a node in second workspace
        let node2 = Box::new(crate::nodes::table::TableViewerNode::new(
            crate::node::NodeId(0), // This will be replaced
            "table2".to_string(),
        ));
        let id2 = stoat1.add_node(node2);

        // IDs should be different (globally unique)
        assert_ne!(id1, id2);
        assert!(
            id1.0 < id2.0,
            "IDs should be increasing: {} < {}",
            id1.0,
            id2.0
        );

        // Save state
        stoat1.save().unwrap();

        // Create new Stoat instance that loads the same state
        let mut stoat2 = {
            let config = StoatConfig {
                state_dir: Some(state_dir),
                workspace: None,
            };
            Stoat::new_with_config(config).unwrap()
        };

        // Add another node - should get next ID after the previous ones
        let node3 = Box::new(crate::nodes::table::TableViewerNode::new(
            crate::node::NodeId(0), // This will be replaced
            "table3".to_string(),
        ));
        let id3 = stoat2.add_node(node3);

        // ID should be greater than both previous IDs
        assert!(
            id3.0 > id1.0 && id3.0 > id2.0,
            "New ID {} should be greater than previous IDs {} and {}",
            id3.0,
            id1.0,
            id2.0
        );
    }

    #[test]
    fn test_state_directory_removal_resets_ids() {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let state_dir = temp_dir.path().to_path_buf();

        // First session: create stoat, add node, save, drop
        let (first_id, cache_dir) = {
            let mut stoat1 = {
                let config = StoatConfig {
                    state_dir: Some(state_dir.clone()),
                    workspace: None,
                };
                Stoat::new_with_config(config).unwrap()
            };

            // Add a table node
            let table_node = Box::new(crate::nodes::table::TableViewerNode::new(
                crate::node::NodeId(0), // This will be replaced
                "test_table_1".to_string(),
            ));
            let id = stoat1.add_node(table_node);
            let cache_dir = stoat1.state.get_cache_dir();

            // Save state
            stoat1.save().unwrap();

            (id, cache_dir)
        }; // stoat1 dropped here

        // Second session: load from same state dir
        let second_id = {
            let mut stoat2 = {
                let config = StoatConfig {
                    state_dir: Some(state_dir.clone()),
                    workspace: None,
                };
                Stoat::new_with_config(config).unwrap()
            };

            // Add another node - should continue from previous ID counter
            let table_node = Box::new(crate::nodes::table::TableViewerNode::new(
                crate::node::NodeId(0), // This will be replaced
                "test_table_2".to_string(),
            ));
            stoat2.add_node(table_node)
        }; // stoat2 dropped here

        // Remove the entire state directory
        std::fs::remove_dir_all(&state_dir).unwrap();

        // Third session: should start fresh with ID 1
        let fresh_id = {
            let mut stoat3 = {
                let config = StoatConfig {
                    state_dir: Some(state_dir.clone()),
                    workspace: None,
                };
                Stoat::new_with_config(config).unwrap()
            };

            // Add node - should start from 1 again
            let table_node = Box::new(crate::nodes::table::TableViewerNode::new(
                crate::node::NodeId(0), // This will be replaced
                "test_table_fresh".to_string(),
            ));
            stoat3.add_node(table_node)
        };

        // Verify ID progression
        assert_eq!(first_id.0, 1, "First node should get ID 1");
        assert!(
            second_id.0 > first_id.0,
            "Second node should get higher ID than first"
        );
        assert_eq!(fresh_id.0, 1, "Fresh start should reset to ID 1");

        // Verify cache directory is under state directory, not hardcoded location
        assert!(
            cache_dir.starts_with(&state_dir),
            "Cache directory {:?} should be under state directory {:?}",
            cache_dir,
            state_dir
        );
        assert!(
            !std::path::Path::new(".stoat_cache").exists(),
            "Should not create .stoat_cache in current directory"
        );
    }

    #[test]
    fn test_table_cache_uses_global_state_directory() {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let custom_state_dir = temp_dir.path().join("custom_stoat");

        let mut stoat = {
            let config = StoatConfig {
                state_dir: Some(custom_state_dir.clone()),
                workspace: None,
            };
            Stoat::new_with_config(config).unwrap()
        };

        // Add a table node
        let table_node = Box::new(crate::nodes::table::TableViewerNode::new(
            crate::node::NodeId(0), // This will be replaced
            "test_table".to_string(),
        ));
        let _id = stoat.add_node(table_node);

        // Cache directory should be under the custom state directory
        let _expected_cache_dir = custom_state_dir.join("cache");

        // Save to ensure directories are created
        stoat.save().unwrap();

        // The cache directory should exist under the custom state directory, not in .stoat_cache
        assert!(
            !std::path::Path::new(".stoat_cache").exists(),
            "Should not create .stoat_cache in current directory"
        );

        // Note: We can't easily verify the cache directory was used since the table node
        // only creates the cache directory when it actually caches data, but we can verify
        // the state directory structure is correct
        assert!(custom_state_dir.exists());
    }
}
