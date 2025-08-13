/// The core implementation the [`Stoat`] editor runtime. Ie the editor minus the GUI/TUI/CLI
/// interfaces.
use input::{Action, Key, ModalConfig, ModalSystem, Mode, UserInput};
use std::path::{Path, PathBuf};
use view::View;
use workspace::Workspace;

pub mod config;
pub mod error;
pub mod log;
pub mod persist {
    // TODO: save state over generic FS. Ideally configurable serialization format.
}
pub mod data;
pub mod graph;
pub mod input;
pub mod mode;
pub mod node;
pub mod nodes;
pub mod value;
pub mod view;
pub mod view_state;
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

            let data_path = state_dir.join("workspaces").join(format!("{name}.ron"));
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
    /// Modal input system
    modal_system: ModalSystem,
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
            message: format!("Failed to load state: {e}"),
        })?;

        // Override active workspace if specified
        if let Some(workspace_name) = &config.workspace {
            state
                .set_active_workspace(workspace_name.clone())
                .map_err(|e| Error::Generic {
                    message: format!("Failed to set workspace: {e}"),
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
            modal_system: ModalSystem::new(),
        })
    }

    pub fn with_workspace(workspace: Workspace) -> Self {
        let state_path = state::default_state_path();
        let state = state::State::default();
        Self {
            active: workspace,
            state,
            state_path,
            modal_system: ModalSystem::new(),
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
                message: format!("Failed to deserialize workspace: {e}"),
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
                message: format!("Failed to serialize workspace: {e}"),
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
                message: format!("Failed to save state: {e}"),
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

    /// Create a node (currently no node types implemented)
    pub fn create_node(
        &mut self,
        node_type: &str,
        _name: String,
        _config: value::Value,
    ) -> Result<node::NodeId> {
        // No node types are currently implemented
        Err(Error::Generic {
            message: format!(
                "Unknown node type: {node_type} - no node types currently implemented"
            ),
        })
    }

    /// Push a user input into Stoat.
    //
    // TODO: UserInput needs to return available actions? For automatic ? and client validation?
    pub fn user_input(&mut self, ue: impl Into<UserInput>) -> Option<Action> {
        let user_input = ue.into();

        // Convert UserInput to Key
        let UserInput::Key(key) = user_input;

        // Process key through modal system
        let action = self.modal_system.process_key(key);

        // Handle the action
        if let Some(ref action) = action {
            match action {
                Action::ExitApp => {
                    // Exit app handling would be done by the caller
                },
                Action::ChangeMode(_) => {
                    // Mode change is handled internally by ModalSystem
                },
                Action::Move(_direction) => {
                    // TODO: Implement movement in workspace/view
                    // self.active.view_mut().move_cursor(direction);
                },
                Action::Delete => {
                    // TODO: Implement delete
                },
                Action::DeleteLine => {
                    // TODO: Implement delete line
                },
                Action::Yank => {
                    // TODO: Implement yank
                },
                Action::Paste => {
                    // TODO: Implement paste
                },
                Action::Jump(_target) => {
                    // TODO: Implement jump navigation
                },
                Action::InsertChar => {
                    // TODO: Implement character insertion
                    // This action would typically insert the last key pressed
                },
                Action::YankLine => {
                    // TODO: Implement yank line
                },
                Action::CommandInput => {
                    // TODO: Implement command mode input
                },
                Action::ExecuteCommand => {
                    // TODO: Execute the current command
                },
                Action::ShowActionList => {
                    // TODO: Show available actions
                },
                Action::ShowCommandPalette => {
                    // TODO: Show command palette
                },
                Action::GatherNodes => {
                    // Gather nodes into the current viewport
                    self.active.view_state_mut().center_on_selected();
                },
            }
        }

        action
    }
    /// Push multiple user events into Stoat.
    pub fn user_inputs<T: Into<UserInput>>(&mut self, user_inputs: impl IntoIterator<Item = T>) {
        for ue in user_inputs {
            self.user_input(ue);
        }
    }
    pub fn view(&self) -> &View {
        self.active.view()
    }

    /// Get the current view state for rendering
    pub fn view_state(&self) -> &view_state::ViewState {
        self.active.view_state()
    }

    /// Get mutable access to view state
    pub fn view_state_mut(&mut self) -> &mut view_state::ViewState {
        self.active.view_state_mut()
    }

    /// Get the node graph
    pub fn graph(&self) -> &graph::NodeGraph {
        self.active.graph()
    }

    /// Get mutable access to the graph
    pub fn graph_mut(&mut self) -> &mut graph::NodeGraph {
        self.active.graph_mut()
    }

    /// Get the current modal mode
    pub fn current_mode(&self) -> &Mode {
        self.modal_system.current_mode()
    }

    /// Get available actions in the current mode
    pub fn available_actions(&self) -> Vec<(&Key, &Action)> {
        self.modal_system.available_actions()
    }

    /// Get formatted keybindings for display (returns key string and action description)
    pub fn get_display_bindings(&self) -> Vec<(String, String)> {
        self.available_actions()
            .into_iter()
            .map(|(key, action)| (key.to_string(), action.to_string()))
            .collect()
    }

    /// Update the modal system (call on each frame/tick)
    pub fn tick(&mut self) {
        self.modal_system.tick();
    }

    /// Load a modal configuration from RON
    pub fn load_modal_config(&mut self, config: ModalConfig) {
        self.modal_system = ModalSystem::with_config(config);
    }

    /// Load modal configuration from a file
    pub fn load_modal_config_from_file(&mut self, path: &Path) -> Result<()> {
        let config = ModalConfig::from_file(path).map_err(|e| Error::Generic {
            message: format!("Failed to load modal config: {e}"),
        })?;
        self.modal_system = ModalSystem::with_config(config);
        Ok(())
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
        let temp_dir = TempDir::new().expect("Failed to create temporary directory for test");
        let state_dir = temp_dir.path().to_path_buf();

        // Create first instance and save state
        let stoat1 = {
            let config = StoatConfig {
                state_dir: Some(state_dir.clone()),
                workspace: None,
            };
            Stoat::new_with_config(config).expect("Failed to create Stoat instance with config")
        };
        stoat1.save().expect("Failed to save Stoat state");

        // Create second instance that loads the saved state
        let stoat2 = {
            let config = StoatConfig {
                state_dir: Some(state_dir),
                workspace: None,
            };
            Stoat::new_with_config(config).expect("Failed to create second Stoat instance")
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
            state_dir: Some(
                TempDir::new()
                    .expect("Failed to create temp directory for invalid workspace test")
                    .path()
                    .to_path_buf(),
            ),
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
        let temp_dir =
            TempDir::new().expect("Failed to create temporary directory for custom state dir test");
        let custom_state_dir = temp_dir.path().join("custom/path");
        let config = StoatConfig {
            state_dir: Some(custom_state_dir),
            workspace: None,
        };
        let stoat =
            Stoat::new_with_config(config).expect("Failed to create Stoat with custom state dir");

        assert_eq!(stoat.state().active_workspace, "default");
        // State should be saved in the custom directory
        stoat
            .save()
            .expect("Failed to save Stoat with custom state dir");
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
        let temp_dir =
            TempDir::new().expect("Failed to create temporary directory for global ID test");
        let state_dir = temp_dir.path().to_path_buf();

        // Create first workspace
        let mut stoat1 = {
            let config = StoatConfig {
                state_dir: Some(state_dir.clone()),
                workspace: None,
            };
            Stoat::new_with_config(config)
                .expect("Failed to create first Stoat instance for global ID test")
        };

        // Allocate IDs directly from state
        let id1 = node::NodeId(stoat1.state_mut().allocate_id());

        // Create second workspace
        stoat1
            .state_mut()
            .add_workspace("workspace2".to_string(), None)
            .expect("Failed to add workspace2");
        stoat1
            .state_mut()
            .set_active_workspace("workspace2".to_string())
            .expect("Failed to set active workspace to workspace2");

        // Allocate another ID
        let id2 = node::NodeId(stoat1.state_mut().allocate_id());

        // IDs should be different (globally unique)
        assert_ne!(id1, id2);
        assert!(
            id1.0 < id2.0,
            "IDs should be increasing: {} < {}",
            id1.0,
            id2.0
        );

        // Save state
        stoat1
            .save()
            .expect("Failed to save state after allocating IDs");

        // Create new Stoat instance that loads the same state
        let mut stoat2 = {
            let config = StoatConfig {
                state_dir: Some(state_dir),
                workspace: None,
            };
            Stoat::new_with_config(config)
                .expect("Failed to create second Stoat instance for global ID test")
        };

        // Allocate another ID - should get next ID after the previous ones
        let id3 = node::NodeId(stoat2.state_mut().allocate_id());

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
        let temp_dir = TempDir::new()
            .expect("Failed to create temporary directory for state directory removal test");
        let state_dir = temp_dir.path().to_path_buf();

        // First session: create stoat, allocate ID, save, drop
        let (first_id, cache_dir) = {
            let mut stoat1 = {
                let config = StoatConfig {
                    state_dir: Some(state_dir.clone()),
                    workspace: None,
                };
                Stoat::new_with_config(config)
                    .expect("Failed to create first Stoat instance for state removal test")
            };

            // Allocate an ID
            let id = node::NodeId(stoat1.state_mut().allocate_id());
            let cache_dir = stoat1.state.get_cache_dir();

            // Save state
            stoat1.save().expect("Failed to save first Stoat instance");

            (id, cache_dir)
        }; // stoat1 dropped here

        // Second session: load from same state dir
        let second_id = {
            let mut stoat2 = {
                let config = StoatConfig {
                    state_dir: Some(state_dir.clone()),
                    workspace: None,
                };
                Stoat::new_with_config(config)
                    .expect("Failed to create second Stoat instance for state removal test")
            };

            // Allocate another ID - should continue from previous ID counter
            node::NodeId(stoat2.state_mut().allocate_id())
        }; // stoat2 dropped here

        // Remove the entire state directory
        std::fs::remove_dir_all(&state_dir).expect("Failed to remove state directory");

        // Third session: should start fresh with ID 1
        let fresh_id = {
            let mut stoat3 = {
                let config = StoatConfig {
                    state_dir: Some(state_dir.clone()),
                    workspace: None,
                };
                Stoat::new_with_config(config)
                    .expect("Failed to create third Stoat instance after state removal")
            };

            // Allocate ID - should start from 1 again
            node::NodeId(stoat3.state_mut().allocate_id())
        };

        // Verify ID progression
        assert_eq!(first_id.0, 1, "First ID should be 1");
        assert!(
            second_id.0 > first_id.0,
            "Second ID should be higher than first"
        );
        assert_eq!(fresh_id.0, 1, "Fresh start should reset to ID 1");

        // Verify cache directory is under state directory, not hardcoded location
        assert!(
            cache_dir.starts_with(&state_dir),
            "Cache directory {cache_dir:?} should be under state directory {state_dir:?}"
        );
        assert!(
            !Path::new(".stoat_cache").exists(),
            "Should not create .stoat_cache in current directory"
        );
    }

    #[test]
    fn test_cache_directory_uses_global_state_directory() {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().expect("Failed to create temporary directory for cache test");
        let custom_state_dir = temp_dir.path().join("custom_stoat");

        let stoat = {
            let config = StoatConfig {
                state_dir: Some(custom_state_dir.clone()),
                workspace: None,
            };
            Stoat::new_with_config(config).expect("Failed to create Stoat instance for cache test")
        };

        // Get the cache directory from state
        let cache_dir = stoat.state().get_cache_dir();

        // Cache directory should be under the custom state directory
        assert!(
            cache_dir.starts_with(&custom_state_dir),
            "Cache directory {cache_dir:?} should be under state directory {custom_state_dir:?}"
        );

        // Save to ensure directories are created
        stoat
            .save()
            .expect("Failed to save Stoat instance for cache test");

        // The cache directory should not be in current directory
        assert!(
            !Path::new(".stoat_cache").exists(),
            "Should not create .stoat_cache in current directory"
        );

        // Verify the state directory structure is correct
        assert!(custom_state_dir.exists());
    }
}
