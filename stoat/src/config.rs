//! Configuration management for Stoat editor.
//!
//! This module provides the foundation for loading and managing global configuration
//! from `config.toml` files. It is used by the application entry point in `stoat_gui::app`
//! to initialize the [`crate::Stoat`] entity with user preferences.
//!
//! # Architecture
//!
//! The configuration system follows this flow:
//!
//! 1. **App startup** (`stoat_gui::app::run_with_paths`) calls [`Config::load_default()`]
//! 2. [`Config::load_default()`] finds the platform-specific config directory using
//!    [`dirs::config_dir()`]
//! 3. If `config.toml` exists, it's loaded via [`Config::load()`]; otherwise defaults are used
//! 4. The [`Config`] is passed to [`crate::Stoat::new()`] during entity creation
//! 5. [`crate::Stoat`] stores the config and uses it to initialize subsystems
//!
//! # File Location
//!
//! The config file location is platform-specific, determined by the `dirs` crate:
//! - **macOS**: `~/Library/Application Support/stoat/config.toml`
//! - **Linux**: `~/.config/stoat/config.toml`
//! - **Windows**: `C:\Users\<user>\AppData\Roaming\stoat\config.toml`
//!
//! # Testing
//!
//! Tests use [`Config::load()`] with explicit paths to temporary directories created
//! by `tempfile::tempdir()`. This avoids relying on the `dirs` library's platform-specific
//! logic, which is tested by that crate. See the `#[cfg(test)]` module for examples.
//!
//! # Future Configuration Options
//!
//! This is currently an empty foundation. Future configuration will be added as fields
//! to the [`Config`] struct, such as:
//! - Editor font family (currently hardcoded in `stoat_gui::editor_style`)
//! - Color themes
//! - Keybindings
//! - LSP server settings
//!
//! # Related
//!
//! - [`crate::Stoat`] - main entity that receives and stores the config
//! - `stoat_gui::app` - application entry point that loads the config
//! - `stoat_gui::editor_style` - currently uses hardcoded values, will use config in future

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Global configuration for Stoat editor.
///
/// Currently an empty foundation for future configuration options. This struct will
/// be extended with fields like font family, color themes, and keybindings as needed.
///
/// # Usage
///
/// At application startup, use [`Config::load_default()`] to load user configuration:
///
/// ```no_run
/// # use stoat::config::Config;
/// let config = Config::load_default().unwrap_or_default();
/// // Pass config to Stoat::new(config, cx)
/// ```
///
/// In tests, use [`Config::load()`] with explicit paths to temporary directories:
///
/// ```
/// # use stoat::config::Config;
/// # use tempfile::tempdir;
/// # use std::fs;
/// let tmp_dir = tempdir().unwrap();
/// let config_path = tmp_dir.path().join("config.toml");
/// fs::write(&config_path, "# empty config").unwrap();
/// let config = Config::load(&config_path).unwrap();
/// ```
///
/// # Deserialization
///
/// The config is loaded from TOML format using [`serde`]. Unknown fields in the
/// config file are ignored to allow forward compatibility.
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
    /// Load configuration from a specific file path.
    ///
    /// This method reads and deserializes a TOML config file from the given path.
    /// It is primarily used in tests with temporary directories, but can also be
    /// used to load configuration from non-standard locations.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file cannot be read (IO error)
    /// - The file contains invalid TOML syntax
    /// - The TOML structure doesn't match the [`Config`] schema
    ///
    /// # Usage
    ///
    /// ```no_run
    /// # use stoat::config::Config;
    /// # use std::path::Path;
    /// let config = Config::load(Path::new("/path/to/config.toml"))?;
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    ///
    /// In tests:
    ///
    /// ```
    /// # use stoat::config::Config;
    /// # use tempfile::tempdir;
    /// # use std::fs;
    /// let tmp_dir = tempdir().unwrap();
    /// let config_path = tmp_dir.path().join("config.toml");
    /// fs::write(&config_path, "# test config").unwrap();
    /// let config = Config::load(&config_path).unwrap();
    /// ```
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: Config = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        Ok(config)
    }

    /// Load configuration from the default platform-specific location.
    ///
    /// This method attempts to load the config from the standard location for the
    /// current platform (see module-level docs for paths). If the config file doesn't
    /// exist, returns [`Config::default()`] instead of an error.
    ///
    /// Called by `stoat_gui::app::run_with_paths` during application startup to
    /// initialize the [`crate::Stoat`] entity with user preferences.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The config file exists but cannot be read (IO error)
    /// - The config file exists but contains invalid TOML
    ///
    /// Does NOT return an error if:
    /// - The config directory doesn't exist (returns default)
    /// - The config file doesn't exist (returns default)
    ///
    /// # Usage
    ///
    /// ```no_run
    /// # use stoat::config::Config;
    /// // At app startup:
    /// let config = Config::load_default().unwrap_or_default();
    /// // Use config to initialize Stoat
    /// ```
    pub fn load_default() -> Result<Self> {
        let Some(config_path) = Self::default_config_path() else {
            // No config directory available, use defaults
            return Ok(Config::default());
        };

        if !config_path.exists() {
            // Config file doesn't exist yet, use defaults
            return Ok(Config::default());
        }

        Self::load(&config_path)
    }

    /// Load configuration with optional path override.
    ///
    /// This method implements the configuration loading priority:
    ///
    /// 1. **Override path** (highest priority) - If provided, loads from this path
    /// 2. **Default location** - Platform-specific config directory
    /// 3. **Built-in defaults** - If no config file exists
    ///
    /// This is the main entry point for loading configuration at application startup,
    /// supporting both CLI argument (`--config`) and environment variable (`STOAT_CONFIG`)
    /// overrides via clap's `env` attribute.
    ///
    /// # Arguments
    ///
    /// * `override_path` - Optional path to a config file, typically from CLI args or env vars
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - An override path is provided but the file doesn't exist
    /// - An override path is provided but contains invalid TOML
    /// - The default config file exists but cannot be read
    /// - The default config file exists but contains invalid TOML
    ///
    /// Does NOT return an error if:
    /// - No override path is provided and the default config doesn't exist (returns defaults)
    ///
    /// # Usage
    ///
    /// Called by `stoat_gui::app::run_with_paths` with the CLI/env var config path:
    ///
    /// ```no_run
    /// # use stoat::config::Config;
    /// # use std::path::PathBuf;
    /// // With override (from CLI --config or STOAT_CONFIG env var)
    /// let config_path = Some(PathBuf::from("/custom/config.toml"));
    /// let config = Config::load_with_overrides(config_path.as_deref())?;
    ///
    /// // Without override (uses platform default or built-in defaults)
    /// let config = Config::load_with_overrides(None)?;
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn load_with_overrides(override_path: Option<&Path>) -> Result<Self> {
        if let Some(path) = override_path {
            // Override path provided - must exist and be valid
            Self::load(path)
        } else {
            // No override - use default behavior (try platform default, fallback to defaults)
            Self::load_default()
        }
    }

    /// Get the default config file path for the current platform.
    ///
    /// Returns the path to `config.toml` in the platform-specific config directory.
    /// The directory location is determined by [`dirs::config_dir()`]:
    ///
    /// - **macOS**: `~/Library/Application Support/stoat/config.toml`
    /// - **Linux**: `~/.config/stoat/config.toml`
    /// - **Windows**: `C:\Users\<user>\AppData\Roaming\stoat\config.toml`
    ///
    /// Returns [`None`] if the config directory cannot be determined (rare, usually
    /// indicates a misconfigured system).
    ///
    /// # Usage
    ///
    /// ```no_run
    /// # use stoat::config::Config;
    /// if let Some(path) = Config::default_config_path() {
    ///     println!("Config should be at: {}", path.display());
    /// }
    /// ```
    pub fn default_config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("stoat").join("config.toml"))
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
    fn load_default_returns_default_when_no_file() {
        // This test doesn't create any config file, so load_default should
        // return the default config without error
        let config = Config::load_default().unwrap();
        assert!(std::matches!(config, Config { .. }));
    }

    #[test]
    fn load_with_overrides_loads_from_override_path() {
        let tmp_dir = tempdir().unwrap();
        let config_path = tmp_dir.path().join("custom_config.toml");
        std::fs::write(&config_path, "# custom config").unwrap();

        let config = Config::load_with_overrides(Some(&config_path)).unwrap();
        assert!(std::matches!(config, Config { .. }));
    }

    #[test]
    fn load_with_overrides_errors_when_override_path_doesnt_exist() {
        let tmp_dir = tempdir().unwrap();
        let config_path = tmp_dir.path().join("nonexistent.toml");

        let result = Config::load_with_overrides(Some(&config_path));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to read"));
    }

    #[test]
    fn load_with_overrides_uses_default_when_no_override() {
        // No override provided, should use default behavior (try default path, fallback to
        // defaults)
        let config = Config::load_with_overrides(None).unwrap();
        assert!(std::matches!(config, Config { .. }));
    }

    #[test]
    fn load_with_overrides_errors_on_invalid_toml_in_override() {
        let tmp_dir = tempdir().unwrap();
        let config_path = tmp_dir.path().join("invalid.toml");
        std::fs::write(&config_path, "invalid toml {{{{").unwrap();

        let result = Config::load_with_overrides(Some(&config_path));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to parse"));
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
