use clap::Parser;
use stoat::cli::{
    config::{Cli, Command},
    node::{AddNodeCommand, NodeCommand},
};
use stoat_core::{Stoat, StoatConfig};

fn handle_node_command(
    node_cmd: NodeCommand,
    _stoat: &Stoat,
) -> Result<(), Box<dyn std::error::Error>> {
    match node_cmd {
        NodeCommand::Add(add_cmd) => {
            match add_cmd {
                AddNodeCommand::Csv {
                    path,
                    name,
                    delimiter,
                    headers,
                } => {
                    println!(
                        "Adding CSV node: path={:?}, name={:?}, delimiter={}, headers={}",
                        path, name, delimiter, headers
                    );
                    // TODO: Implement CSV node creation
                },
                AddNodeCommand::Table { name, max_rows } => {
                    println!(
                        "Adding Table node: name={:?}, max_rows={:?}",
                        name, max_rows
                    );
                    // TODO: Implement Table node creation
                },
                AddNodeCommand::Json { path, name } => {
                    println!("Adding JSON node: path={:?}, name={:?}", path, name);
                    // TODO: Implement JSON node creation
                },
                AddNodeCommand::Api {
                    url,
                    name,
                    method,
                    headers,
                } => {
                    println!(
                        "Adding API node: url={}, name={:?}, method={:?}, headers={:?}",
                        url, name, method, headers
                    );
                    // TODO: Implement API node creation
                },
            }
        },
        NodeCommand::List {
            detailed,
            type_filter,
        } => {
            println!(
                "Listing nodes: detailed={}, type_filter={:?}",
                detailed, type_filter
            );
            // TODO: Implement node listing
        },
        NodeCommand::Show { node, outputs } => {
            println!("Showing node '{}': outputs={}", node, outputs);
            // TODO: Implement node details display
        },
        NodeCommand::Exec { node, format, save } => {
            println!(
                "Executing node '{}': format={:?}, save={:?}",
                node, format, save
            );
            // TODO: Implement node execution
        },
        NodeCommand::Remove { node, force } => {
            println!("Removing node '{}': force={}", node, force);
            // TODO: Implement node removal
        },
        NodeCommand::Config { node, config, data } => {
            println!(
                "Configuring node '{}': config={:?}, data={:?}",
                node, config, data
            );
            // TODO: Implement node configuration
        },
    }
    Ok(())
}

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
        Command::Node(node_cmd) => handle_node_command(node_cmd, &stoat),
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

    #[test]
    fn test_stoat_initialization_with_default_config() {
        let (stoat, _temp_dir) = Stoat::test();
        assert_eq!(stoat.state().active_workspace, "default");

        // Verify state was properly initialized
        stoat.save().unwrap();
    }

    #[test]
    fn test_stoat_initialization_with_custom_state_dir() {
        let temp_dir = TempDir::new().unwrap();
        let custom_state_dir = temp_dir.path().join("custom");
        let config = StoatConfig {
            state_dir: Some(custom_state_dir),
            workspace: None,
        };
        let stoat = Stoat::new_with_config(config).unwrap();

        stoat.save().unwrap();

        // The workspace file should have been created when we saved
        let workspace_path = &stoat.state().current_workspace().unwrap().data_path;
        assert!(workspace_path.exists());
    }

    #[test]
    fn test_workspace_switching_error_handling() {
        // This should fail because we're trying to switch to a non-existent workspace
        let temp_dir = TempDir::new().unwrap();
        let config = StoatConfig {
            state_dir: Some(temp_dir.path().to_path_buf()),
            workspace: Some("nonexistent".to_string()),
        };

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
        let (stoat, _temp_dir) = Stoat::test_with_workspace("test_workspace");

        assert_eq!(stoat.state().active_workspace, "test_workspace");
        assert!(stoat.state().workspaces.contains_key("test_workspace"));
    }

    #[test]
    fn test_state_persistence_across_instances() {
        let temp_dir = TempDir::new().unwrap();
        let state_dir = temp_dir.path().to_path_buf();

        // Create first instance and save some state
        let mut stoat1 = {
            let config = StoatConfig {
                state_dir: Some(state_dir.clone()),
                workspace: None,
            };
            Stoat::new_with_config(config).unwrap()
        };
        stoat1
            .state_mut()
            .add_workspace("persistent_test".to_string(), None)
            .unwrap();
        stoat1.save().unwrap();

        // Create second instance and verify state was loaded
        let stoat2 = {
            let config = StoatConfig {
                state_dir: Some(state_dir),
                workspace: None,
            };
            Stoat::new_with_config(config).unwrap()
        };
        assert!(stoat2.state().workspaces.contains_key("persistent_test"));
        assert_eq!(
            stoat1.state().workspaces.len(),
            stoat2.state().workspaces.len()
        );
    }
}
