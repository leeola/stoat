use clap::Parser;
use stoat::cli::config::{Cli, Command};
use stoat_core::{Stoat, StoatConfig};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Initialize Stoat with configuration from CLI
    let config = StoatConfig {
        state_dir: cli.state_dir,
        workspace: cli.workspace,
    };

    let stoat = Stoat::new_with_config(config).unwrap_or_else(|e| {
        eprintln!("Error: Failed to initialize Stoat: {}", e);
        std::process::exit(1);
    });

    // Execute command
    let result: Result<(), Box<dyn std::error::Error>> = match cli.command {
        // Command::Workspace(workspace_cmd) => {
        //     println!("Workspace command: {:?}", workspace_cmd);
        //     // TODO: Implement workspace command handling
        // },
        // Command::Node(node_cmd) => {
        //     println!("Node command: {:?}", node_cmd);
        //     // TODO: Implement node command handling
        // },
        // Command::Link(link_args) => {
        //     println!("Link command: {:?}", link_args);
        //     // TODO: Implement link command handling
        // },
        // Command::Run(run_args) => {
        //     println!("Run command: {:?}", run_args);
        //     // TODO: Implement run command handling
        // },
        Command::Csv(csv_cmd) => {
            println!("CSV command: {:?}", csv_cmd);
            // TODO: Implement CSV command handling
            Ok(())
        },
        // Command::Status(status_args) => {
        //     println!("Status command: {:?}", status_args);
        //     // TODO: Implement status command handling
        // },
        // Command::Repl => {
        //     println!("Starting REPL mode...");
        //     // TODO: Implement REPL mode
        //     Ok(())
        // },
    };

    // Handle command result and save state
    if let Err(e) = result {
        eprintln!("Command failed: {}", e);
        std::process::exit(1);
    }

    // Save state and workspace
    if let Err(e) = stoat.save() {
        eprintln!("Warning: Failed to save: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_config(temp_dir: &TempDir, workspace: Option<String>) -> StoatConfig {
        StoatConfig {
            state_dir: Some(temp_dir.path().to_path_buf()),
            workspace,
        }
    }

    #[test]
    fn test_stoat_initialization_with_default_config() {
        let temp_dir = TempDir::new().unwrap();
        let config = create_test_config(&temp_dir, None);

        let stoat = Stoat::new_with_config(config).unwrap();
        assert_eq!(stoat.state().active_workspace, "default");
        
        // Verify state file was created
        assert!(temp_dir.path().join("state.ron").exists());
    }

    #[test]
    fn test_stoat_initialization_with_custom_state_dir() {
        let temp_dir = TempDir::new().unwrap();
        let custom_state_dir = temp_dir.path().join("custom");
        
        let config = StoatConfig {
            state_dir: Some(custom_state_dir.clone()),
            workspace: None,
        };

        let stoat = Stoat::new_with_config(config).unwrap();
        stoat.save().unwrap();
        
        // Verify state file was created in custom directory
        assert!(custom_state_dir.join("state.ron").exists());
        
        // The workspace file should have been created when we saved
        let workspace_path = &stoat.state().current_workspace().unwrap().data_path;
        assert!(workspace_path.exists());
    }

    #[test]
    fn test_workspace_switching_error_handling() {
        let temp_dir = TempDir::new().unwrap();
        let config = create_test_config(&temp_dir, Some("nonexistent".to_string()));

        let result = Stoat::new_with_config(config);
        assert!(result.is_err());
        
        // Verify the error message contains workspace information
        if let Err(error) = result {
            let error_msg = error.to_string();
            assert!(error_msg.contains("nonexistent"));
        }
    }

    #[test]
    fn test_workspace_switching_success() {
        let temp_dir = TempDir::new().unwrap();
        
        // First, create a stoat instance and add a workspace
        let config1 = create_test_config(&temp_dir, None);
        let mut stoat1 = Stoat::new_with_config(config1).unwrap();
        stoat1.state_mut().add_workspace("test_workspace".to_string(), Some("Test workspace".to_string())).unwrap();
        stoat1.save().unwrap();

        // Now create a new instance that switches to the test workspace
        let config2 = create_test_config(&temp_dir, Some("test_workspace".to_string()));
        let stoat2 = Stoat::new_with_config(config2).unwrap();
        
        assert_eq!(stoat2.state().active_workspace, "test_workspace");
        assert!(stoat2.state().workspaces.contains_key("test_workspace"));
    }

    #[test]
    fn test_state_persistence_across_instances() {
        let temp_dir = TempDir::new().unwrap();
        let config = create_test_config(&temp_dir, None);

        // Create first instance and save some state
        let mut stoat1 = Stoat::new_with_config(config.clone()).unwrap();
        stoat1.state_mut().add_workspace("persistent_test".to_string(), None).unwrap();
        stoat1.save().unwrap();

        // Create second instance and verify state was loaded
        let stoat2 = Stoat::new_with_config(config).unwrap();
        assert!(stoat2.state().workspaces.contains_key("persistent_test"));
        assert_eq!(stoat1.state().workspaces.len(), stoat2.state().workspaces.len());
    }
}
