use std::path::PathBuf;
use stoat::cli::node::{AddNodeCommand, NodeCommand};
use stoat_core::{node::NodeId, Stoat};

/// Configuration enum for different node types
#[derive(Debug)]
#[allow(dead_code)]
enum NodeConfig {
    Csv {
        path: PathBuf,
        #[allow(dead_code)]
        delimiter: char,
        #[allow(dead_code)]
        headers: bool,
    },
    Table {
        #[allow(dead_code)]
        max_rows: Option<usize>,
    },
    Json {
        path: PathBuf,
    },
    #[allow(dead_code)]
    Api {
        url: String,
        method: String,
        headers: Vec<String>,
    },
}

/// Generic node addition function
fn add_node(
    stoat: &mut Stoat,
    node_type: &str,
    name: Option<String>,
    config: NodeConfig,
) -> Result<NodeId, Box<dyn std::error::Error>> {
    let node_name = name.unwrap_or_else(|| format!("{}_node", node_type));

    // Convert NodeConfig to Value
    let config_value = match config {
        NodeConfig::Csv { path, .. } => {
            // For CSV nodes, just pass the path as a string
            stoat_core::value::Value::String(path.to_string_lossy().into())
        },
        NodeConfig::Table { max_rows: _ } => {
            // For table nodes, pass empty config - cache configuration will be added by create_node
            stoat_core::value::Value::Empty
        },
        NodeConfig::Json { path } => {
            // For JSON nodes, just pass the path as a string
            stoat_core::value::Value::String(path.to_string_lossy().into())
        },
        NodeConfig::Api { .. } => {
            return Err("API nodes are not yet implemented".into());
        },
    };

    let id = stoat.create_node(node_type, node_name.clone(), config_value)?;

    println!(
        "✓ Added {} node '{}' with ID {:?}",
        node_type, node_name, id
    );
    Ok(id)
}

pub fn handle(node_cmd: NodeCommand, stoat: &mut Stoat) -> Result<(), Box<dyn std::error::Error>> {
    match node_cmd {
        NodeCommand::Add(add_cmd) => match add_cmd {
            AddNodeCommand::Csv {
                path,
                name,
                delimiter,
                headers,
            } => {
                let config = NodeConfig::Csv {
                    path,
                    delimiter,
                    headers,
                };
                add_node(stoat, "csv", name, config)?;
            },
            AddNodeCommand::Table { name, max_rows } => {
                let config = NodeConfig::Table { max_rows };
                add_node(stoat, "table", name, config)?;
            },
            AddNodeCommand::Json { path, name } => {
                let config = NodeConfig::Json { path };
                add_node(stoat, "json", name, config)?;
            },
            AddNodeCommand::Api {
                url: _,
                name: _,
                method: _,
                headers: _,
            } => {
                return Err("API nodes are not yet implemented".into());
            },
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use stoat::cli::node::{AddNodeCommand, NodeCommand};
    use stoat_core::Stoat;

    #[test]
    fn test_node_listing_functionality() {
        let (mut stoat, _temp_dir) = Stoat::test();

        // Initially workspace should be empty
        let nodes = stoat.workspace().list_nodes();
        assert_eq!(nodes.len(), 0);

        // Add some test nodes
        let id1 = stoat
            .create_node(
                "table",
                "sales_table".to_string(),
                stoat_core::value::Value::Empty,
            )
            .unwrap();
        let id2 = stoat
            .create_node(
                "table",
                "users_table".to_string(),
                stoat_core::value::Value::Empty,
            )
            .unwrap();

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

    #[test]
    fn test_table_node_creation_via_command() {
        let (mut stoat, _temp_dir) = Stoat::test();

        // Initially workspace should be empty
        assert_eq!(stoat.workspace().list_nodes().len(), 0);

        // Create a table node command
        let add_cmd = AddNodeCommand::Table {
            name: Some("test_table".to_string()),
            max_rows: Some(100),
        };
        let node_cmd = NodeCommand::Add(add_cmd);

        // Execute the command
        let result = handle(node_cmd, &mut stoat);
        assert!(result.is_ok());

        // Verify node was added
        let nodes = stoat.workspace().list_nodes();
        assert_eq!(nodes.len(), 1);

        let (_, node) = &nodes[0];
        assert_eq!(node.name(), "test_table");
        assert_eq!(node.node_type(), stoat_core::node::NodeType::TableViewer);
    }

    #[test]
    fn test_csv_node_creation_via_command() {
        let (mut stoat, _temp_dir) = Stoat::test();

        // Initially workspace should be empty
        assert_eq!(stoat.workspace().list_nodes().len(), 0);

        // Create a CSV node command
        let add_cmd = AddNodeCommand::Csv {
            path: PathBuf::from("test.csv"),
            name: Some("csv_data".to_string()),
            delimiter: ',',
            headers: true,
        };
        let node_cmd = NodeCommand::Add(add_cmd);

        // Execute the command
        let result = handle(node_cmd, &mut stoat);
        assert!(result.is_ok());

        // Verify node was added
        let nodes = stoat.workspace().list_nodes();
        assert_eq!(nodes.len(), 1);

        let (_, node) = &nodes[0];
        assert_eq!(node.name(), "csv_data");
        assert_eq!(node.node_type(), stoat_core::node::NodeType::CsvSource);
    }

    #[test]
    fn test_json_node_creation_via_command() {
        let (mut stoat, _temp_dir) = Stoat::test();

        // Initially workspace should be empty
        assert_eq!(stoat.workspace().list_nodes().len(), 0);

        // Create a JSON node command
        let add_cmd = AddNodeCommand::Json {
            path: PathBuf::from("test.json"),
            name: Some("json_data".to_string()),
        };
        let node_cmd = NodeCommand::Add(add_cmd);

        // Execute the command
        let result = handle(node_cmd, &mut stoat);
        assert!(result.is_ok());

        // Verify node was added
        let nodes = stoat.workspace().list_nodes();
        assert_eq!(nodes.len(), 1);

        let (_, node) = &nodes[0];
        assert_eq!(node.name(), "json_data");
        assert_eq!(node.node_type(), stoat_core::node::NodeType::JsonSource);
    }

    #[test]
    fn test_multiple_node_creation() {
        let (mut stoat, _temp_dir) = Stoat::test();

        // Create multiple nodes of different types
        let commands = vec![
            NodeCommand::Add(AddNodeCommand::Table {
                name: Some("table1".to_string()),
                max_rows: None,
            }),
            NodeCommand::Add(AddNodeCommand::Csv {
                path: PathBuf::from("data.csv"),
                name: Some("csv1".to_string()),
                delimiter: ';',
                headers: false,
            }),
            NodeCommand::Add(AddNodeCommand::Json {
                path: PathBuf::from("config.json"),
                name: None, // Test default naming
            }),
        ];

        // Execute all commands
        for cmd in commands {
            let result = handle(cmd, &mut stoat);
            assert!(result.is_ok());
        }

        // Verify all nodes were added
        let nodes = stoat.workspace().list_nodes();
        assert_eq!(nodes.len(), 3);

        // Check node names and types
        let node_names: Vec<&str> = nodes.iter().map(|(_, node)| node.name()).collect();
        assert!(node_names.contains(&"table1"));
        assert!(node_names.contains(&"csv1"));
        assert!(node_names.contains(&"json_node")); // Default name

        // Check types
        let mut type_counts = std::collections::HashMap::new();
        for (_, node) in &nodes {
            *type_counts.entry(node.node_type()).or_insert(0) += 1;
        }
        assert_eq!(
            type_counts.get(&stoat_core::node::NodeType::TableViewer),
            Some(&1)
        );
        assert_eq!(
            type_counts.get(&stoat_core::node::NodeType::CsvSource),
            Some(&1)
        );
        assert_eq!(
            type_counts.get(&stoat_core::node::NodeType::JsonSource),
            Some(&1)
        );
    }

    #[test]
    fn test_api_node_not_implemented() {
        let (mut stoat, _temp_dir) = Stoat::test();

        // Try to create an API node
        let add_cmd = AddNodeCommand::Api {
            url: "https://api.example.com".to_string(),
            name: Some("api_test".to_string()),
            method: stoat::cli::node::HttpMethod::Get,
            headers: vec!["Authorization=Bearer token".to_string()],
        };
        let node_cmd = NodeCommand::Add(add_cmd);

        // Execute the command - should fail
        let result = handle(node_cmd, &mut stoat);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not yet implemented"));

        // Verify no node was added
        let nodes = stoat.workspace().list_nodes();
        assert_eq!(nodes.len(), 0);
    }

    #[test]
    fn test_node_factory_functions() {
        let (mut stoat, _temp_dir) = stoat_core::Stoat::test();

        // Test CSV node creation
        let csv_config_value = stoat_core::value::Value::String("test.csv".into());
        let csv_result = stoat.create_node("csv", "test_csv".to_string(), csv_config_value);
        assert!(csv_result.is_ok());
        let csv_id = csv_result.unwrap();
        let nodes = stoat.workspace().list_nodes();
        let csv_node = nodes.iter().find(|(id, _)| *id == csv_id).unwrap().1;
        assert_eq!(csv_node.name(), "test_csv");

        // Test Table node creation
        let table_config_value = stoat_core::value::Value::Empty;
        let table_result = stoat.create_node("table", "table_node".to_string(), table_config_value);
        assert!(table_result.is_ok());
        let table_id = table_result.unwrap();
        let nodes = stoat.workspace().list_nodes();
        let table_node = nodes.iter().find(|(id, _)| *id == table_id).unwrap().1;
        assert_eq!(table_node.name(), "table_node");

        // Test JSON node creation
        let json_config_value = stoat_core::value::Value::String("data.json".into());
        let json_result = stoat.create_node("json", "my_json".to_string(), json_config_value);
        assert!(json_result.is_ok());
        let json_id = json_result.unwrap();
        let nodes = stoat.workspace().list_nodes();
        let json_node = nodes.iter().find(|(id, _)| *id == json_id).unwrap().1;
        assert_eq!(json_node.name(), "my_json");

        // Test unknown node type
        let unknown_result = stoat.create_node(
            "unknown",
            "unknown_node".to_string(),
            stoat_core::value::Value::Empty,
        );
        assert!(unknown_result.is_err());
        assert!(unknown_result
            .unwrap_err()
            .to_string()
            .contains("Unknown node type"));
    }

    #[test]
    fn test_node_persistence_after_save_and_load() {
        let (mut stoat, temp_dir) = stoat_core::Stoat::test();

        // Add a node using the new create_node method
        let csv_config_value = stoat_core::value::Value::String("test.csv".into());
        let node_id = stoat
            .create_node("csv", "test_node".to_string(), csv_config_value)
            .unwrap();

        // Verify node exists before save
        let nodes_before_save = stoat.workspace().list_nodes();
        assert_eq!(nodes_before_save.len(), 1);
        assert_eq!(nodes_before_save[0].0, node_id);

        // Save and reload
        stoat.save().unwrap();
        let config = stoat_core::StoatConfig {
            state_dir: Some(temp_dir.path().to_path_buf()),
            workspace: None,
        };
        let reloaded_stoat = stoat_core::Stoat::new_with_config(config).unwrap();

        // FIXED: Nodes are now properly serialized AND reconstructed via NodeInit registry
        let nodes_after_reload = reloaded_stoat.workspace().list_nodes();
        assert_eq!(
            nodes_after_reload.len(),
            1,
            "Nodes should be properly reconstructed after save/load"
        );

        // Verify the reconstructed node has the same properties
        assert_eq!(nodes_after_reload[0].0, node_id);
        assert_eq!(nodes_after_reload[0].1.name(), "test_node");
        assert_eq!(nodes_after_reload[0].1.node_type().to_string(), "csv");
    }
}
