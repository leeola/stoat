/// The core implementation the [`Stoat`] editor runtime. Ie the editor minus the GUI/TUI/CLI
/// interfaces.
use input::{
    Action, CommandInfoState, HelpDisplayState, HelpType, Key, ModalConfig, ModalSystem, Mode,
    UserInput,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use view::View;
use workspace::Workspace;

pub mod config;
pub mod error;
pub mod log;
pub mod persist {
    // TODO: save state over generic FS. Ideally configurable serialization format.
}
pub mod buffer_manager;
pub mod command;
pub mod data;
pub mod input;
pub mod mode;
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
    /// Command registry for named command execution
    commands: command::CommandRegistry,
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
            commands: command::CommandRegistry::with_builtins(),
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
            commands: command::CommandRegistry::with_builtins(),
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

    /// Create a buffer in the active workspace
    pub fn create_buffer(&mut self, name: String) -> buffer_manager::BufferId {
        self.active.create_buffer(name)
    }

    /// Create a buffer from a file
    pub fn create_buffer_from_file(&mut self, path: PathBuf) -> Result<buffer_manager::BufferId> {
        self.active.create_buffer_from_file(path)
    }

    /// Create a buffer with content
    pub fn create_buffer_with_content(
        &mut self,
        name: String,
        content: String,
    ) -> buffer_manager::BufferId {
        self.active.create_buffer_with_content(name, content)
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
                Action::GatherNodes => {
                    // Gather nodes into the current viewport
                    self.active.view_state_mut().center_on_selected();
                },
                Action::AlignNodes => {
                    // AlignNodes no longer applies in buffer-centric model
                    // This action is kept for compatibility but does nothing
                },
                Action::ShowHelp => {
                    // Show help action - GUI will handle displaying modal
                },
                Action::ShowActionHelp(_) => {
                    // Show action help - GUI will handle displaying modal
                },
                Action::ShowModeHelp(_) => {
                    // Show mode help - GUI will handle displaying modal
                },
                Action::ExecuteCommand(name, args) => {
                    // Execute named command using internal method to avoid borrowing conflicts
                    let _result = self.execute_command_internal(name, args.clone());
                    // TODO: Handle command result (display, error handling)
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

    /// Get the buffer manager
    pub fn buffers(&self) -> &buffer_manager::BufferManager {
        self.active.buffers()
    }

    /// Get mutable access to the buffer manager
    pub fn buffers_mut(&mut self) -> &mut buffer_manager::BufferManager {
        self.active.buffers_mut()
    }

    /// Get the command registry
    pub fn commands(&self) -> &command::CommandRegistry {
        &self.commands
    }

    /// Get mutable access to the command registry
    pub fn commands_mut(&mut self) -> &mut command::CommandRegistry {
        &mut self.commands
    }

    /// Execute a command by name with arguments
    pub fn execute_command(&mut self, name: &str, args: Vec<value::Value>) -> Result<value::Value> {
        // Use the internal execute method to avoid borrowing conflicts
        self.execute_command_internal(name, args)
    }

    /// Internal command execution to avoid borrowing conflicts
    fn execute_command_internal(
        &mut self,
        name: &str,
        args: Vec<value::Value>,
    ) -> Result<value::Value> {
        // We need to temporarily take ownership of the registry to avoid borrowing conflicts
        let commands = std::mem::replace(&mut self.commands, command::CommandRegistry::new());
        let result = {
            let mut context = command::CommandContext::new(self);
            commands.execute_command(name, &mut context, args)
        };
        // Put the commands back
        self.commands = commands;
        result
    }

    /// Get the current modal mode
    pub fn current_mode(&self) -> &Mode {
        self.modal_system.current_mode()
    }

    /// Get access to the modal system
    pub fn modal_system(&self) -> &ModalSystem {
        &self.modal_system
    }

    /// Get command input state for GUI display
    pub fn command_input_state(&self) -> &input::modal::CommandInputState {
        self.modal_system.command_input()
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

    /// Get help information for current mode
    pub fn get_help_info(&self) -> Vec<(String, String, String)> {
        // If in help mode, show actions for the targeted mode or previous mode
        if self.current_mode() == &Mode::Help {
            // First check if we've navigated to a specific mode's help
            if let Some(target_mode) = self.modal_system.help_target_mode() {
                return self.get_help_info_for_mode(target_mode);
            }
            // Fall back to previous mode if no target set
            if let Some(previous_mode) = self.modal_system.previous_mode() {
                return self.get_help_info_for_mode(previous_mode);
            }
        }

        self.available_actions()
            .into_iter()
            .map(|(key, action)| {
                (
                    key.to_string(),
                    action.to_string(),
                    action.description().to_string(),
                )
            })
            .collect()
    }

    /// Get help information for a specific mode
    pub fn get_help_info_for_mode(&self, mode: &Mode) -> Vec<(String, String, String)> {
        self.modal_system
            .available_actions_for_mode(mode)
            .into_iter()
            .map(|(key, action)| {
                (
                    key.to_string(),
                    action.to_string(),
                    action.description().to_string(),
                )
            })
            .collect()
    }

    /// Get extended help for a specific action
    pub fn get_extended_help(&self, key_str: &str) -> Option<String> {
        self.available_actions()
            .into_iter()
            .find(|(key, _)| key.to_string() == key_str)
            .map(|(_, action)| action.extended_description())
    }

    /// Get action information (name and extended help) for a specific key
    pub fn get_action_info(&self, key_str: &str) -> Option<(String, String)> {
        self.available_actions()
            .into_iter()
            .find(|(key, _)| key.to_string() == key_str)
            .map(|(_, action)| (action.to_string(), action.extended_description()))
    }

    /// Get complete help display state for GUI rendering
    pub fn get_help_state(&self) -> HelpDisplayState {
        // Help is visible when in Help mode
        let visible = self.current_mode() == &Mode::Help;

        if !visible {
            return HelpDisplayState::default();
        }

        // Determine what mode's help we're showing
        let target_mode = self
            .modal_system
            .help_target_mode()
            .or_else(|| self.modal_system.previous_mode())
            .unwrap_or(&Mode::Normal);

        // Get the commands for the target mode
        let commands = self.get_help_info_for_mode(target_mode);

        // Check if we're showing action-specific help
        if self.modal_system.showing_action_help() {
            if let Some(action_key) = self.modal_system.current_action_help() {
                let (action_name, extended_help) = self
                    .get_action_info(action_key)
                    .unwrap_or_else(|| (action_key.to_string(), "No help available".to_string()));

                HelpDisplayState {
                    visible: true,
                    help_type: HelpType::Action,
                    title: format!("Help: {action_name}"),
                    commands,
                    extended_help: Some(extended_help),
                }
            } else {
                // Fallback if no action is tracked
                HelpDisplayState {
                    visible: true,
                    help_type: HelpType::Action,
                    title: "Action Help".to_string(),
                    commands,
                    extended_help: Some("No specific action help available".to_string()),
                }
            }
        } else {
            HelpDisplayState {
                visible: true,
                help_type: HelpType::Mode,
                title: format!(
                    "{} Mode",
                    target_mode
                        .as_str()
                        .chars()
                        .next()
                        .expect("Mode string should not be empty")
                        .to_uppercase()
                        .collect::<String>()
                        + &target_mode.as_str()[1..]
                ),
                commands,
                extended_help: None,
            }
        }
    }

    /// Get command info display state for GUI rendering
    pub fn get_command_info_state(&self) -> CommandInfoState {
        let bindings = self.get_display_bindings();

        CommandInfoState {
            visible: true,
            mode_name: self.current_mode().as_str().to_string(),
            commands: bindings.into_iter().take(5).collect(), // Limit to 5 most relevant
        }
    }

    /// Execute keys using Vim-like notation and return a snapshot of the resulting state
    pub fn execute(&mut self, notation: &str) -> Result<ModeSnapshot, String> {
        let keys = input::notation::parse_keys(notation)?;

        // Execute all keys
        for key in keys {
            self.user_input(key);
        }

        Ok(self.snapshot())
    }

    /// Take a snapshot of current state
    pub fn snapshot(&self) -> ModeSnapshot {
        ModeSnapshot {
            mode: self.current_mode().clone(),
            commands: self.get_command_map(),
            previous_mode: self.modal_system.previous_mode().cloned(),
            help_target_mode: self.modal_system.help_target_mode().cloned(),
        }
    }

    /// Get command map for current state
    fn get_command_map(&self) -> CommandMap {
        // The available_actions method now handles help mode logic internally
        let actions = self.available_actions();

        let entries = actions
            .into_iter()
            .map(|(key, action)| {
                let command = Command {
                    action: action.clone(),
                    description: action.description().to_string(),
                };
                (key.clone(), command)
            })
            .collect();

        CommandMap { entries }
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

/// Snapshot of the modal state for testing
#[derive(Debug, Clone, PartialEq)]
pub struct ModeSnapshot {
    pub mode: Mode,
    pub commands: CommandMap,
    pub previous_mode: Option<Mode>,
    pub help_target_mode: Option<Mode>,
}

/// Map of available commands for testing
#[derive(Debug, Clone, PartialEq)]
pub struct CommandMap {
    entries: HashMap<Key, Command>,
}

/// A command with its action and description
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub action: Action,
    pub description: String,
}

impl CommandMap {
    /// Check if a key exists
    pub fn has(&self, key: &Key) -> bool {
        self.entries.contains_key(key)
    }

    /// Get command for a key
    pub fn get(&self, key: &Key) -> Option<&Command> {
        self.entries.get(key)
    }

    /// Check if an action is bound to any key
    pub fn has_action(&self, action: &Action) -> bool {
        self.entries.values().any(|cmd| cmd.action == *action)
    }

    /// Get all keys that trigger an action
    pub fn keys_for(&self, action: &Action) -> Vec<&Key> {
        self.entries
            .iter()
            .filter(|(_, cmd)| cmd.action == *action)
            .map(|(key, _)| key)
            .collect()
    }

    /// Total number of commands
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Assert specific commands exist with correct bindings
    pub fn assert_has(&self, expected: &[(Key, Action)]) {
        for (key, action) in expected {
            match self.get(key) {
                Some(cmd) => assert_eq!(
                    cmd.action, *action,
                    "Key {:?} maps to {:?}, expected {:?}",
                    key, cmd.action, action
                ),
                None => panic!(
                    "Key {:?} not found in commands. Available: {:?}",
                    key,
                    self.entries.keys().collect::<Vec<_>>()
                ),
            }
        }
    }
}

impl ModeSnapshot {
    /// Debug print for test failures
    pub fn debug_print(&self) {
        println!("=== Mode Snapshot ===");
        println!("Current mode: {:?}", self.mode);
        println!("Previous mode: {:?}", self.previous_mode);
        println!("Commands ({} total):", self.commands.len());

        let mut sorted: Vec<_> = self.commands.entries.iter().collect();
        sorted.sort_by_key(|(k, _)| format!("{k:?}"));

        for (key, cmd) in sorted {
            println!("  {:?} -> {:?}", key, cmd.action);
        }
        println!("===================");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use input::{ModifiedKey, NamedKey};
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
        let id1 = buffer_manager::BufferId(stoat1.state_mut().allocate_id());

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
        let id2 = buffer_manager::BufferId(stoat1.state_mut().allocate_id());

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
        let id3 = buffer_manager::BufferId(stoat2.state_mut().allocate_id());

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
            let id = buffer_manager::BufferId(stoat1.state_mut().allocate_id());
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
            buffer_manager::BufferId(stoat2.state_mut().allocate_id())
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
            buffer_manager::BufferId(stoat3.state_mut().allocate_id())
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

    // Test reproducing the help mode bug
    #[test]
    fn test_help_mode_shows_canvas_commands() {
        let mut stoat = Stoat::new();

        // Enter Canvas mode then Help mode using either notation:
        // "c?" (convenience) or "c<S-/>" (explicit)
        let snapshot = stoat
            .execute("c?")
            .expect("Should execute canvas help sequence");

        // Verify we're in Help mode
        assert_eq!(snapshot.mode, Mode::Help);

        // Verify previous mode is Canvas
        assert_eq!(snapshot.previous_mode, Some(Mode::Canvas));

        // DEBUG: Print what we actually get
        snapshot.debug_print();

        // Verify we have Canvas commands visible
        let commands = &snapshot.commands;

        // Test specific Canvas commands are present with correct actions
        commands.assert_has(&[
            (Key::Char('a'), Action::GatherNodes),
            (Key::Named(NamedKey::Esc), Action::ChangeMode(Mode::Normal)),
            (Key::Modified(ModifiedKey::Shift('/')), Action::ShowHelp),
        ]);

        // Also verify count
        assert!(
            commands.len() >= 3,
            "Expected at least 3 Canvas commands, got {}",
            commands.len()
        );
    }

    #[test]
    fn test_help_mode_interactive_key_lookup() {
        let mut stoat = Stoat::new();

        // Enter help from Canvas
        let snapshot = stoat
            .execute("c?")
            .expect("Should execute canvas help sequence");
        assert_eq!(snapshot.mode, Mode::Help);

        // Press 'a' in help mode - should trigger ShowActionHelp
        let action = stoat.user_input(Key::Char('a'));
        assert_eq!(
            action,
            Some(Action::ShowActionHelp("a".to_string())),
            "Key 'a' in Help mode should show action help"
        );
    }

    #[test]
    fn test_help_escape_returns_to_previous() {
        let mut stoat = Stoat::new();

        // c? enters Canvas then Help, <Esc> navigates to Normal help
        stoat
            .execute("c?<Esc>")
            .expect("Should execute canvas help then navigate to normal help");

        // Second <Esc> should return to Canvas (original mode)
        let snapshot = stoat
            .execute("<Esc>")
            .expect("Should exit help mode and return to Canvas");

        assert_eq!(
            snapshot.mode,
            Mode::Canvas,
            "Esc from Help should return to Canvas"
        );

        // Verify Canvas commands are back
        snapshot
            .commands
            .assert_has(&[(Key::Char('a'), Action::GatherNodes)]);
    }

    #[test]
    fn test_snapshot_command_queries() {
        let mut stoat = Stoat::new();

        // Get Canvas snapshot
        let canvas = stoat.execute("c").expect("Should enter canvas mode");

        // Test various query methods
        assert!(canvas.commands.has(&Key::Char('a')));
        assert!(canvas.commands.has_action(&Action::GatherNodes));

        let gather_keys = canvas.commands.keys_for(&Action::GatherNodes);
        assert_eq!(gather_keys, vec![&Key::Char('a')]);

        // Test command details
        let cmd = canvas
            .commands
            .get(&Key::Char('a'))
            .expect("Should have 'a' command in canvas mode");
        assert_eq!(cmd.action, Action::GatherNodes);
        assert!(cmd.description.contains("Gather"));
    }

    #[test]
    fn test_help_modal_shows_friendly_key_format() {
        let mut stoat = Stoat::new();

        // Get Canvas mode help
        stoat.execute("c").expect("Should enter canvas mode");
        let help_info = stoat.get_help_info();

        // Find the help key binding - should show as "?"
        let help_key = help_info
            .iter()
            .find(|(_, action, _)| action.contains("help"))
            .map(|(key, _, _)| key.clone());

        assert_eq!(
            help_key,
            Some("?".to_string()),
            "Should display ? not Shift+/"
        );

        // Find escape key - should show as "<Esc>"
        let esc_key = help_info
            .iter()
            .find(|(_, action, _)| action.contains("normal"))
            .map(|(key, _, _)| key.clone());

        assert_eq!(esc_key, Some("<Esc>".to_string()), "Should display <Esc>");

        // Find gather key - should show as "a"
        let gather_key = help_info
            .iter()
            .find(|(_, action, _)| action.contains("Gather"))
            .map(|(key, _, _)| key.clone());

        assert_eq!(gather_key, Some("a".to_string()), "Should display a");
    }

    #[test]
    fn test_help_action_uses_friendly_key_format() {
        let mut stoat = Stoat::new();

        // Enter help mode from Canvas
        stoat.execute("c?").expect("Should enter canvas help mode");
        assert_eq!(stoat.current_mode(), &Mode::Help);

        // Press '?' in help mode to see help for the help action
        let action = stoat.user_input(Key::Modified(ModifiedKey::Shift('/')));

        // Should get ShowActionHelp with friendly format "?" not "Shift('/')"
        assert_eq!(
            action,
            Some(Action::ShowActionHelp("?".to_string())),
            "ShowActionHelp should use friendly format '?' not 'Shift(/)'"
        );
    }

    #[test]
    fn test_help_mode_navigation_to_canvas() {
        let mut stoat = Stoat::new();

        // Enter help from Normal mode, then navigate to Canvas help
        let snapshot = stoat
            .execute("?c")
            .expect("Should execute normal help then canvas navigation");

        // Should be in help mode
        assert_eq!(snapshot.mode, Mode::Help);

        // Should be showing Canvas mode help
        assert_eq!(snapshot.help_target_mode, Some(Mode::Canvas));

        // Should have Canvas commands visible
        snapshot.commands.assert_has(&[
            (Key::Char('a'), Action::GatherNodes),
            (Key::Named(NamedKey::Esc), Action::ChangeMode(Mode::Normal)),
            (Key::Modified(ModifiedKey::Shift('/')), Action::ShowHelp),
        ]);
    }

    #[test]
    fn test_help_mode_navigation_from_normal() {
        let mut stoat = Stoat::new();

        // Enter help from Normal mode, then navigate to Canvas help
        let snapshot = stoat
            .execute("?c")
            .expect("Should execute normal help then canvas navigation");

        // Should be in help mode
        assert_eq!(snapshot.mode, Mode::Help);

        // Should be showing Canvas mode help
        assert_eq!(snapshot.help_target_mode, Some(Mode::Canvas));

        // Should have Canvas commands visible
        snapshot.commands.assert_has(&[
            (Key::Named(NamedKey::Esc), Action::ChangeMode(Mode::Normal)),
            (Key::Modified(ModifiedKey::Shift('/')), Action::ShowHelp),
            (Key::Char('a'), Action::GatherNodes),
        ]);
    }

    #[test]
    fn test_help_mode_navigation_between_modes() {
        let mut stoat = Stoat::new();

        // Start in Canvas mode, enter help
        stoat.execute("c?").expect("Should enter canvas help");

        // Navigate to Normal mode help (Esc is bound to ChangeMode(Normal) in Canvas)
        let action = stoat.user_input(Key::Named(NamedKey::Esc));
        assert_eq!(action, Some(Action::ShowModeHelp(Mode::Normal)));

        // Should now show Normal mode commands
        let snapshot = stoat.snapshot();
        assert_eq!(snapshot.help_target_mode, Some(Mode::Normal));

        // Navigate back to Canvas mode help
        let action = stoat.user_input(Key::Char('c'));
        assert_eq!(action, Some(Action::ShowModeHelp(Mode::Canvas)));

        // Should now show Canvas mode commands
        let snapshot = stoat.snapshot();
        assert_eq!(snapshot.help_target_mode, Some(Mode::Canvas));
    }

    #[test]
    fn test_help_mode_escape_returns_to_original_mode() {
        let mut stoat = Stoat::new();

        // Start in Canvas mode, enter help, navigate to Normal help
        stoat
            .execute("c?<Esc>")
            .expect("Should enter canvas help then navigate to normal help");

        // Verify we're showing Normal help but came from Canvas
        let snapshot = stoat.snapshot();
        assert_eq!(snapshot.mode, Mode::Help);
        assert_eq!(snapshot.previous_mode, Some(Mode::Canvas));
        assert_eq!(snapshot.help_target_mode, Some(Mode::Normal));

        // Escape should return to Canvas (original mode), not Normal
        let snapshot = stoat
            .execute("<Esc>")
            .expect("Should escape back to original mode");

        assert_eq!(snapshot.mode, Mode::Canvas);
        assert_eq!(snapshot.help_target_mode, None);
    }

    #[test]
    fn test_help_mode_action_lookup_in_targeted_mode() {
        let mut stoat = Stoat::new();

        // Enter Normal help, navigate to Canvas help
        stoat
            .execute("?c")
            .expect("Should enter help and navigate to canvas");

        // Press 'a' which should show help for GatherNodes (Canvas action)
        let action = stoat.user_input(Key::Char('a'));
        assert_eq!(action, Some(Action::ShowActionHelp("a".to_string())));

        // Escape from action help should return to Canvas mode help
        let action = stoat.user_input(Key::Named(NamedKey::Esc));
        assert_eq!(action, Some(Action::ShowModeHelp(Mode::Canvas)));

        // Navigate to Normal mode help (Canvas's Esc binding)
        let action = stoat.user_input(Key::Named(NamedKey::Esc));
        assert_eq!(action, Some(Action::ShowModeHelp(Mode::Normal)));

        // Now pressing 'a' should be ignored since Normal mode has no 'a' binding
        let action = stoat.user_input(Key::Char('a'));
        assert_eq!(action, None); // No binding for 'a' in Normal mode
    }

    #[test]
    fn test_help_single_esc_from_normal_mode() {
        let mut stoat = Stoat::new();

        // From Normal mode, enter help then immediately exit with single Esc
        let snapshot = stoat
            .execute("?<Esc>")
            .expect("Should enter help then exit with single Esc");

        // Should be back in Normal mode after just one Esc
        assert_eq!(snapshot.mode, Mode::Normal);
        assert_eq!(snapshot.help_target_mode, None);
    }

    #[test]
    fn test_action_help_esc_behavior() {
        let mut stoat = Stoat::new();

        // Navigate to Canvas help: ?c
        stoat.execute("?c").expect("Should enter Canvas help");

        // Show action help: a
        let action = stoat.user_input(Key::Char('a'));
        assert_eq!(action, Some(Action::ShowActionHelp("a".to_string())));

        // First Esc should return to Canvas help
        let action = stoat.user_input(Key::Named(NamedKey::Esc));
        assert_eq!(action, Some(Action::ShowModeHelp(Mode::Canvas)));

        // Second Esc should navigate to Normal help (Canvas's Esc binding)
        let action = stoat.user_input(Key::Named(NamedKey::Esc));
        assert_eq!(action, Some(Action::ShowModeHelp(Mode::Normal)));
    }

    #[test]
    fn test_action_help_esc_with_extra_action() {
        let mut stoat = Stoat::new();

        // Same sequence as the failing test
        stoat.execute("?c").expect("Should enter Canvas help");

        let action = stoat.user_input(Key::Char('a'));
        assert_eq!(action, Some(Action::ShowActionHelp("a".to_string())));

        let action = stoat.user_input(Key::Named(NamedKey::Esc));
        assert_eq!(action, Some(Action::ShowModeHelp(Mode::Canvas)));

        // Press 'a' again (this is the extra step in the failing test)
        let action = stoat.user_input(Key::Char('a'));
        assert_eq!(action, Some(Action::ShowActionHelp("a".to_string())));

        // Now Esc again
        let action = stoat.user_input(Key::Named(NamedKey::Esc));
        assert_eq!(action, Some(Action::ShowModeHelp(Mode::Canvas))); // This is what's actually
                                                                      // happening
    }
}
