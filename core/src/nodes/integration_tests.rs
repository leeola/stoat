//! Integration tests for data processing pipelines
//!
//! This module tests the interaction between different node types
//! and demonstrates real-world data processing scenarios.

#[cfg(test)]
mod tests {
    use crate::{
        node::{Node, NodeId},
        nodes::map::{MapNode, MapOperation},
        value::Value,
        workspace::Workspace,
    };
    use std::collections::HashMap;

    /// Test CSV -> Map pipeline
    #[cfg(feature = "csv")]
    #[test]
    fn csv_to_map_pipeline() {
        use crate::value::{Array, Map};
        use compact_str::CompactString;

        // Create mock CSV data
        let mut csv_data = Vec::new();

        // Row 1: name=Alice, age=25, department=Engineering
        let mut row1 = indexmap::IndexMap::new();
        row1.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Alice")),
        );
        row1.insert(
            CompactString::from("age"),
            Value::String(CompactString::from("25")),
        );
        row1.insert(
            CompactString::from("department"),
            Value::String(CompactString::from("Engineering")),
        );
        csv_data.push(Value::Map(Map(row1)));

        // Row 2: name=Bob, age=30, department=Sales
        let mut row2 = indexmap::IndexMap::new();
        row2.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Bob")),
        );
        row2.insert(
            CompactString::from("age"),
            Value::String(CompactString::from("30")),
        );
        row2.insert(
            CompactString::from("department"),
            Value::String(CompactString::from("Sales")),
        );
        csv_data.push(Value::Map(Map(row2)));

        let csv_array = Value::Array(Array(csv_data));

        // Create MapNode to extract just names
        let mut map_node = MapNode::new(
            NodeId(1),
            "extract_names".to_string(),
            MapOperation::ExtractColumn {
                field: "name".to_string(),
            },
        );

        let mut inputs = HashMap::new();
        inputs.insert("data".to_string(), csv_array);

        let result = map_node.execute(&inputs).unwrap();
        let output = result.get("data").unwrap();

        if let Value::Array(Array(names)) = output {
            assert_eq!(names.len(), 2);
            assert!(matches!(names[0], Value::String(_)));
            assert!(matches!(names[1], Value::String(_)));
            println!(
                "CSV->Map pipeline: Extracted {} names from CSV data",
                names.len()
            );
        } else {
            panic!("Expected array of names");
        }
    }

    /// Test JSON -> Map pipeline
    #[cfg(feature = "json")]
    #[test]
    fn json_to_map_pipeline() {
        use crate::value::{Array, Map};
        use compact_str::CompactString;

        // Create mock JSON data with nested structure
        let mut json_data = Vec::new();

        // Object 1: nested user data
        let mut obj1 = indexmap::IndexMap::new();
        obj1.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Alice")),
        );

        let mut profile1 = indexmap::IndexMap::new();
        profile1.insert(CompactString::from("age"), Value::I64(25));
        profile1.insert(
            CompactString::from("department"),
            Value::String(CompactString::from("Engineering")),
        );
        obj1.insert(CompactString::from("profile"), Value::Map(Map(profile1)));
        json_data.push(Value::Map(Map(obj1)));

        // Object 2: nested user data
        let mut obj2 = indexmap::IndexMap::new();
        obj2.insert(
            CompactString::from("name"),
            Value::String(CompactString::from("Bob")),
        );

        let mut profile2 = indexmap::IndexMap::new();
        profile2.insert(CompactString::from("age"), Value::I64(30));
        profile2.insert(
            CompactString::from("department"),
            Value::String(CompactString::from("Sales")),
        );
        obj2.insert(CompactString::from("profile"), Value::Map(Map(profile2)));
        json_data.push(Value::Map(Map(obj2)));

        let json_array = Value::Array(Array(json_data));

        // Create MapNode to flatten the nested structure
        let mut map_node = MapNode::new(
            NodeId(1),
            "flatten_json".to_string(),
            MapOperation::FlattenObject {
                separator: "_".to_string(),
            },
        );

        let mut inputs = HashMap::new();
        inputs.insert("data".to_string(), json_array);

        let result = map_node.execute(&inputs).unwrap();
        let output = result.get("data").unwrap();

        if let Value::Array(Array(flattened)) = output {
            assert_eq!(flattened.len(), 2);

            if let Value::Map(Map(map)) = &flattened[0] {
                assert!(map.contains_key("profile_age"));
                assert!(map.contains_key("profile_department"));
                assert!(!map.contains_key("profile")); // Nested object should be gone
                println!("JSON->Map pipeline: Flattened nested JSON structure");
            } else {
                panic!("Expected flattened map");
            }
        } else {
            panic!("Expected array of flattened objects");
        }
    }

    /// Test multi-step Map pipeline
    #[test]
    fn multi_step_map_pipeline() {
        use crate::value::{Array, Map};
        use compact_str::CompactString;

        let mut workspace = Workspace::new();

        // Create test data
        let mut test_data = Vec::new();

        let mut row1 = indexmap::IndexMap::new();
        row1.insert(
            CompactString::from("first_name"),
            Value::String(CompactString::from("alice")),
        );
        row1.insert(
            CompactString::from("last_name"),
            Value::String(CompactString::from("smith")),
        );
        row1.insert(CompactString::from("age"), Value::I64(25));
        test_data.push(Value::Map(Map(row1)));

        let mut row2 = indexmap::IndexMap::new();
        row2.insert(
            CompactString::from("first_name"),
            Value::String(CompactString::from("bob")),
        );
        row2.insert(
            CompactString::from("last_name"),
            Value::String(CompactString::from("jones")),
        );
        row2.insert(CompactString::from("age"), Value::I64(30));
        test_data.push(Value::Map(Map(row2)));

        let source_data = Value::Array(Array(test_data));

        // Step 1: Add computed field for full name
        let map_node1 = Box::new(MapNode::new(
            NodeId(1),
            "add_full_name".to_string(),
            MapOperation::AddComputedField {
                field_name: "full_name".to_string(),
                expression: "{first_name} {last_name}".to_string(),
            },
        ));
        let map_id1 = workspace.add_node(map_node1);

        // Step 2: Uppercase all string values
        let map_node2 = Box::new(MapNode::new(
            NodeId(2),
            "uppercase_strings".to_string(),
            MapOperation::TransformValues {
                target_type: crate::nodes::map::ValueType::String,
                transformation: crate::nodes::map::ValueTransformation::ToUppercase,
            },
        ));
        let map_id2 = workspace.add_node(map_node2);

        // Step 3: Select only relevant columns
        let map_node3 = Box::new(MapNode::new(
            NodeId(3),
            "select_final_columns".to_string(),
            MapOperation::SelectColumns {
                fields: vec!["full_name".to_string(), "age".to_string()],
            },
        ));
        let map_id3 = workspace.add_node(map_node3);

        // Link the nodes: data -> step1 -> step2 -> step3
        workspace
            .link_nodes(map_id1, "data".to_string(), map_id2, "data".to_string())
            .unwrap();

        workspace
            .link_nodes(map_id2, "data".to_string(), map_id3, "data".to_string())
            .unwrap();

        // Execute the first node manually to start the pipeline
        let mut inputs = HashMap::new();
        inputs.insert("data".to_string(), source_data);

        // Simulate execution of the pipeline by manually executing each step
        let step1_result = {
            let _node = workspace.get_node(map_id1).unwrap();
            let mut map_node = MapNode::new(
                NodeId(1),
                "add_full_name".to_string(),
                MapOperation::AddComputedField {
                    field_name: "full_name".to_string(),
                    expression: "{first_name} {last_name}".to_string(),
                },
            );
            map_node.execute(&inputs).unwrap()
        };

        let step2_result = {
            let mut map_node = MapNode::new(
                NodeId(2),
                "uppercase_strings".to_string(),
                MapOperation::TransformValues {
                    target_type: crate::nodes::map::ValueType::String,
                    transformation: crate::nodes::map::ValueTransformation::ToUppercase,
                },
            );
            map_node.execute(&step1_result).unwrap()
        };

        let final_result = {
            let mut map_node = MapNode::new(
                NodeId(3),
                "select_final_columns".to_string(),
                MapOperation::SelectColumns {
                    fields: vec!["full_name".to_string(), "age".to_string()],
                },
            );
            map_node.execute(&step2_result).unwrap()
        };

        let output = final_result.get("data").unwrap();

        if let Value::Array(Array(final_rows)) = output {
            assert_eq!(final_rows.len(), 2);

            if let Value::Map(Map(map)) = &final_rows[0] {
                assert_eq!(map.len(), 2); // Should only have full_name and age
                assert!(map.contains_key("full_name"));
                assert!(map.contains_key("age"));

                // Check that full_name was computed and uppercased
                if let Some(Value::String(full_name)) = map.get("full_name") {
                    assert!(full_name.contains("ALICE")); // Should be uppercased
                    assert!(full_name.contains("SMITH"));
                } else {
                    panic!("Expected full_name to be a string");
                }

                println!(
                    "Multi-step pipeline: Successfully processed data through 3 MapNode steps"
                );
            } else {
                panic!("Expected final result to be a map");
            }
        } else {
            panic!("Expected array of final results");
        }
    }
}
