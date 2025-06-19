use stoat::cli::node::{AddNodeCommand, NodeCommand};
use stoat_core::Stoat;

pub fn handle(node_cmd: NodeCommand, stoat: &Stoat) -> Result<(), Box<dyn std::error::Error>> {
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

#[cfg(test)]
mod tests {
    use stoat_core::{node::NodeId, nodes::table::TableViewerNode, Stoat};

    #[test]
    fn test_node_listing_functionality() {
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
