pub mod commands;

#[cfg(test)]
mod tests {
    use stoat_core::{Stoat, StoatConfig};
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
