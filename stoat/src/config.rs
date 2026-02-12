//! Configuration management for Stoat editor.
//!
//! Loads `config.toml` from the path discovered by [`crate::paths::discover`],
//! with optional CLI override via `--config`.
//!
//! # Architecture
//!
//! 1. **App startup** calls [`crate::paths::discover`] to find the `.stoat/` directory
//! 2. [`Config::load_with_overrides`] picks the config path: CLI override > discovered > defaults
//! 3. The [`Config`] is passed to [`crate::Stoat::new()`] during entity creation
//!
//! # Testing
//!
//! Tests use [`Config::load()`] with explicit paths to temporary directories.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// Global configuration for Stoat editor, loaded from `config.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Font family for the editor buffer text.
    ///
    /// Defaults to "Courier" which is available cross-platform (macOS, Linux, Windows).
    /// Common alternatives include "Monaco", "Menlo", "Consolas", or "JetBrains Mono".
    #[serde(default = "default_buffer_font_family")]
    pub buffer_font_family: String,

    /// Font size for the editor buffer text in points.
    ///
    /// Defaults to 15.0 to match Zed's default buffer font size.
    #[serde(default = "default_buffer_font_size")]
    pub buffer_font_size: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            buffer_font_family: default_buffer_font_family(),
            buffer_font_size: default_buffer_font_size(),
        }
    }
}

fn default_buffer_font_family() -> String {
    "Courier".to_string()
}

fn default_buffer_font_size() -> f32 {
    15.0
}

impl Config {
    /// Read and deserialize a TOML config file from the given path.
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: Config = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        Ok(config)
    }

    /// Load configuration with priority: CLI override > discovered path > defaults.
    pub fn load_with_overrides(
        cli_override: Option<&Path>,
        discovered_path: Option<&Path>,
    ) -> Result<Self> {
        if let Some(path) = cli_override {
            return Self::load(path);
        }
        if let Some(path) = discovered_path {
            return Self::load(path);
        }
        Self::load_embedded()
    }

    fn load_embedded() -> Result<Self> {
        let source = include_str!("../../config.toml");
        toml::from_str(source).context("Failed to parse embedded config.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn loads_empty_config() {
        let tmp_dir = tempdir().unwrap();
        let config_path = tmp_dir.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let config = Config::load(&config_path).unwrap();
        // Config should load successfully even when empty
        assert!(std::matches!(config, Config { .. }));
    }

    #[test]
    fn loads_config_with_comments() {
        let tmp_dir = tempdir().unwrap();
        let config_path = tmp_dir.path().join("config.toml");
        std::fs::write(&config_path, "# This is a comment\n# Another comment\n").unwrap();

        let config = Config::load(&config_path).unwrap();
        assert!(std::matches!(config, Config { .. }));
    }

    #[test]
    fn errors_on_invalid_toml() {
        let tmp_dir = tempdir().unwrap();
        let config_path = tmp_dir.path().join("config.toml");
        std::fs::write(&config_path, "invalid toml {{{{").unwrap();

        let result = Config::load(&config_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to parse"));
    }

    #[test]
    fn errors_on_nonexistent_file() {
        let tmp_dir = tempdir().unwrap();
        let config_path = tmp_dir.path().join("nonexistent.toml");

        let result = Config::load(&config_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to read"));
    }

    #[test]
    fn default_is_valid() {
        let config = Config::default();
        // Default config should be constructible
        assert!(std::matches!(config, Config { .. }));
    }

    #[test]
    fn cli_override_takes_priority() {
        let tmp_dir = tempdir().unwrap();
        let cli_path = tmp_dir.path().join("cli.toml");
        let discovered_path = tmp_dir.path().join("discovered.toml");
        std::fs::write(&cli_path, "buffer_font_size = 20.0").unwrap();
        std::fs::write(&discovered_path, "buffer_font_size = 30.0").unwrap();

        let config = Config::load_with_overrides(Some(&cli_path), Some(&discovered_path)).unwrap();
        assert_eq!(config.buffer_font_size, 20.0);
    }

    #[test]
    fn discovered_path_used_when_no_cli_override() {
        let tmp_dir = tempdir().unwrap();
        let discovered_path = tmp_dir.path().join("discovered.toml");
        std::fs::write(&discovered_path, "buffer_font_size = 30.0").unwrap();

        let config = Config::load_with_overrides(None, Some(&discovered_path)).unwrap();
        assert_eq!(config.buffer_font_size, 30.0);
    }

    #[test]
    fn defaults_when_no_paths() {
        let config = Config::load_with_overrides(None, None).unwrap();
        assert_eq!(config.buffer_font_family, "Courier");
        assert_eq!(config.buffer_font_size, 15.0);
    }

    #[test]
    fn load_with_overrides_errors_on_missing_cli_override() {
        let tmp_dir = tempdir().unwrap();
        let missing = tmp_dir.path().join("nonexistent.toml");

        let result = Config::load_with_overrides(Some(&missing), None);
        assert!(result.is_err());
    }

    #[test]
    fn load_with_overrides_errors_on_invalid_toml() {
        let tmp_dir = tempdir().unwrap();
        let bad_path = tmp_dir.path().join("invalid.toml");
        std::fs::write(&bad_path, "invalid toml {{{{").unwrap();

        let result = Config::load_with_overrides(Some(&bad_path), None);
        assert!(result.is_err());
    }

    #[test]
    fn default_font_settings() {
        let config = Config::default();
        assert_eq!(config.buffer_font_family, "Courier");
        assert_eq!(config.buffer_font_size, 15.0);
    }

    #[test]
    fn loads_custom_font_settings() {
        let tmp_dir = tempdir().unwrap();
        let config_path = tmp_dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
buffer_font_family = "JetBrains Mono"
buffer_font_size = 18.0
"#,
        )
        .unwrap();

        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.buffer_font_family, "JetBrains Mono");
        assert_eq!(config.buffer_font_size, 18.0);
    }

    #[test]
    fn uses_defaults_for_missing_font_fields() {
        let tmp_dir = tempdir().unwrap();
        let config_path = tmp_dir.path().join("config.toml");
        std::fs::write(&config_path, "# config with no font fields\n").unwrap();

        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.buffer_font_family, "Courier");
        assert_eq!(config.buffer_font_size, 15.0);
    }
}
