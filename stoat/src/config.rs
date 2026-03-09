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

use crate::fs::Fs;
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
    pub async fn load(path: &Path, fs: &dyn Fs) -> Result<Self> {
        let contents = fs
            .read_to_string(path)
            .await
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: Config = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        Ok(config)
    }

    /// Load configuration with priority: CLI override > discovered path > defaults.
    pub async fn load_with_overrides(
        cli_override: Option<&Path>,
        discovered_path: Option<&Path>,
        fs: &dyn Fs,
    ) -> Result<Self> {
        if let Some(path) = cli_override {
            return Self::load(path, fs).await;
        }
        if let Some(path) = discovered_path {
            return Self::load(path, fs).await;
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
    use crate::fs::FakeFs;
    use std::path::PathBuf;

    #[test]
    fn loads_empty_config() {
        smol::block_on(async {
            let fs = FakeFs::new();
            fs.insert_file("/fake/config.toml", "");
            let config = Config::load(Path::new("/fake/config.toml"), &fs)
                .await
                .unwrap();
            assert!(std::matches!(config, Config { .. }));
        });
    }

    #[test]
    fn loads_config_with_comments() {
        smol::block_on(async {
            let fs = FakeFs::new();
            fs.insert_file(
                "/fake/config.toml",
                "# This is a comment\n# Another comment\n",
            );
            let config = Config::load(Path::new("/fake/config.toml"), &fs)
                .await
                .unwrap();
            assert!(std::matches!(config, Config { .. }));
        });
    }

    #[test]
    fn errors_on_invalid_toml() {
        smol::block_on(async {
            let fs = FakeFs::new();
            fs.insert_file("/fake/config.toml", "invalid toml {{{{");
            let result = Config::load(Path::new("/fake/config.toml"), &fs).await;
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("Failed to parse"));
        });
    }

    #[test]
    fn errors_on_nonexistent_file() {
        smol::block_on(async {
            let fs = FakeFs::new();
            let result = Config::load(Path::new("/fake/nonexistent.toml"), &fs).await;
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("Failed to read"));
        });
    }

    #[test]
    fn default_is_valid() {
        let config = Config::default();
        assert!(std::matches!(config, Config { .. }));
    }

    #[test]
    fn cli_override_takes_priority() {
        smol::block_on(async {
            let fs = FakeFs::new();
            let cli_path = PathBuf::from("/fake/cli.toml");
            let discovered_path = PathBuf::from("/fake/discovered.toml");
            fs.insert_file(&cli_path, "buffer_font_size = 20.0");
            fs.insert_file(&discovered_path, "buffer_font_size = 30.0");

            let config = Config::load_with_overrides(Some(&cli_path), Some(&discovered_path), &fs)
                .await
                .unwrap();
            assert_eq!(config.buffer_font_size, 20.0);
        });
    }

    #[test]
    fn discovered_path_used_when_no_cli_override() {
        smol::block_on(async {
            let fs = FakeFs::new();
            let discovered_path = PathBuf::from("/fake/discovered.toml");
            fs.insert_file(&discovered_path, "buffer_font_size = 30.0");

            let config = Config::load_with_overrides(None, Some(&discovered_path), &fs)
                .await
                .unwrap();
            assert_eq!(config.buffer_font_size, 30.0);
        });
    }

    #[test]
    fn defaults_when_no_paths() {
        smol::block_on(async {
            let fs = FakeFs::new();
            let config = Config::load_with_overrides(None, None, &fs).await.unwrap();
            assert_eq!(config.buffer_font_family, "Courier");
            assert_eq!(config.buffer_font_size, 15.0);
        });
    }

    #[test]
    fn load_with_overrides_errors_on_missing_cli_override() {
        smol::block_on(async {
            let fs = FakeFs::new();
            let missing = PathBuf::from("/fake/nonexistent.toml");
            let result = Config::load_with_overrides(Some(&missing), None, &fs).await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn load_with_overrides_errors_on_invalid_toml() {
        smol::block_on(async {
            let fs = FakeFs::new();
            let bad_path = PathBuf::from("/fake/invalid.toml");
            fs.insert_file(&bad_path, "invalid toml {{{{");
            let result = Config::load_with_overrides(Some(&bad_path), None, &fs).await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn default_font_settings() {
        let config = Config::default();
        assert_eq!(config.buffer_font_family, "Courier");
        assert_eq!(config.buffer_font_size, 15.0);
    }

    #[test]
    fn loads_custom_font_settings() {
        smol::block_on(async {
            let fs = FakeFs::new();
            fs.insert_file(
                "/fake/config.toml",
                "buffer_font_family = \"JetBrains Mono\"\nbuffer_font_size = 18.0\n",
            );
            let config = Config::load(Path::new("/fake/config.toml"), &fs)
                .await
                .unwrap();
            assert_eq!(config.buffer_font_family, "JetBrains Mono");
            assert_eq!(config.buffer_font_size, 18.0);
        });
    }

    #[test]
    fn uses_defaults_for_missing_font_fields() {
        smol::block_on(async {
            let fs = FakeFs::new();
            fs.insert_file("/fake/config.toml", "# config with no font fields\n");
            let config = Config::load(Path::new("/fake/config.toml"), &fs)
                .await
                .unwrap();
            assert_eq!(config.buffer_font_family, "Courier");
            assert_eq!(config.buffer_font_size, 15.0);
        });
    }
}
