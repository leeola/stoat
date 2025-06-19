use clap::Parser;
use stoat::cli::{
    config::{Cli, Command},
    node::{AddNodeCommand, NodeCommand},
};
use stoat_core::{Stoat, StoatConfig};

fn handle_node_command(
    node_cmd: NodeCommand,
    stoat: &Stoat,
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
                    let node_name = name.unwrap_or_else(|| "table_viewer".to_string());
                    println!(
                        "Adding Table node: name={}, max_rows={:?}",
                        node_name, max_rows
                    );

                    // Create a table viewer node
                    let _node = Box::new(stoat_core::nodes::table::TableViewerNode::new(
                        stoat_core::node::NodeId(0), // Will be replaced by workspace
                        node_name.clone(),
                    ));

                    // This would need mutable access to stoat, which we don't have here
                    // For now, just show what would happen
                    println!("Created table viewer node '{}' (note: actual addition to workspace requires mutable access)", node_name);
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
            let nodes = stoat.workspace().list_nodes();

            if nodes.is_empty() {
                println!("No nodes found in workspace");
                return Ok(());
            }

            // Filter nodes by type if specified
            let filtered_nodes: Vec<_> = if let Some(ref filter) = type_filter {
                nodes
                    .into_iter()
                    .filter(|(_, node)| {
                        let node_type = node.node_type();
                        match filter {
                            stoat::cli::node::NodeTypeFilter::Csv => {
                                format!("{:?}", node_type).contains("Csv")
                            },
                            stoat::cli::node::NodeTypeFilter::Json => {
                                format!("{:?}", node_type).contains("Json")
                            },
                            stoat::cli::node::NodeTypeFilter::Table => {
                                matches!(node_type, stoat_core::node::NodeType::TableViewer)
                            },
                            stoat::cli::node::NodeTypeFilter::Api => false, // No API node type yet
                            stoat::cli::node::NodeTypeFilter::Transform => {
                                matches!(node_type, stoat_core::node::NodeType::Map)
                            },
                        }
                    })
                    .collect()
            } else {
                nodes
            };

            if filtered_nodes.is_empty() {
                if let Some(filter) = type_filter {
                    println!("No nodes found matching filter: {:?}", filter);
                } else {
                    println!("No nodes found in workspace");
                }
                return Ok(());
            }

            // Display nodes
            if detailed {
                for (id, node) in filtered_nodes {
                    println!("Node ID: {:?}", id);
                    println!("  Name: {}", node.name());
                    println!("  Type: {:?}", node.node_type());
                    println!(
                        "  Input Ports: {:?}",
                        node.input_ports()
                            .iter()
                            .map(|p| &p.name)
                            .collect::<Vec<_>>()
                    );
                    println!(
                        "  Output Ports: {:?}",
                        node.output_ports()
                            .iter()
                            .map(|p| &p.name)
                            .collect::<Vec<_>>()
                    );
                    println!();
                }
            } else {
                println!("{:<8} {:<20} {:<15}", "ID", "Name", "Type");
                println!("{:-<8} {:-<20} {:-<15}", "", "", "");
                for (id, node) in filtered_nodes {
                    println!(
                        "{:<8} {:<20} {:<15?}",
                        format!("{:?}", id.0),
                        node.name(),
                        node.node_type()
                    );
                }
            }
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

    #[test]
    fn test_node_listing_functionality() {
        use stoat_core::{node::NodeId, nodes::table::TableViewerNode};

        let (mut stoat, _temp_dir) = Stoat::test();

        // Initially workspace should be empty
        let nodes = stoat.workspace().list_nodes();
        assert_eq!(nodes.len(), 0);

        // Add some test nodes
        let table_node1 = Box::new(TableViewerNode::new(NodeId(1), "sales_table".to_string()));
        let table_node2 = Box::new(TableViewerNode::new(NodeId(2), "users_table".to_string()));

        let id1 = stoat.workspace_mut().add_node(table_node1);
        let id2 = stoat.workspace_mut().add_node(table_node2);

        // Verify nodes were added
        let nodes = stoat.workspace().list_nodes();
        assert_eq!(nodes.len(), 2);

        // Test that we can retrieve node details
        let (found_id1, node1) = nodes.iter().find(|(id, _)| *id == id1).unwrap();
        let (found_id2, node2) = nodes.iter().find(|(id, _)| *id == id2).unwrap();

        assert_eq!(*found_id1, id1);
        assert_eq!(*found_id2, id2);
        assert!(node1.name() == "sales_table" || node1.name() == "users_table");
        assert!(node2.name() == "sales_table" || node2.name() == "users_table");
        assert_ne!(node1.name(), node2.name());

        // Test that all nodes are TableViewer type
        for (_, node) in &nodes {
            assert_eq!(node.node_type(), stoat_core::node::NodeType::TableViewer);
        }

        println!("Node listing test passed - found {} nodes", nodes.len());
    }
}
