//! TOML configuration for stoatty.
//!
//! [`load`] settles the embedded default (the repo-root `stoatty.toml`) under
//! the user's file at `<XDG_CONFIG_HOME>/stoatty/config.toml`: a missing user
//! file leaves the defaults in place, and a present one overrides only the
//! fields it sets.

use etcetera::{base_strategy::Xdg, BaseStrategy};
use serde::Deserialize;
use snafu::{ResultExt, Snafu};
use std::{io, path::PathBuf};

/// The default configuration, embedded from the repo root so a built binary
/// carries it without the source tree.
const DEFAULT_CONFIG: &str = include_str!("../../stoatty.toml");

/// The settled stoatty configuration.
///
/// Every field is optional in a user file; an omitted field keeps the embedded
/// default's value.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Config {
    /// Font size in pixels the renderer rasterizes glyphs at.
    #[serde(default)]
    pub font_size: u32,
}

/// An error loading the stoatty configuration.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum ConfigError {
    #[snafu(display("could not read the config file at {}", path.display()))]
    Read {
        path: PathBuf,
        source: io::Error,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("the config file is not valid TOML"))]
    Parse {
        source: toml::de::Error,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("the config does not match the expected fields"))]
    Deserialize {
        source: toml::de::Error,
        #[snafu(implicit)]
        location: snafu::Location,
    },
}

/// Load the settled configuration: the embedded default overlaid with the
/// user's file, if present.
///
/// A missing user file is not an error. A malformed one (invalid TOML, or
/// values that do not fit the schema) returns a [`ConfigError`] rather than
/// panicking.
pub fn load() -> Result<Config, ConfigError> {
    settle(DEFAULT_CONFIG, read_user_config()?.as_deref())
}

/// The user config path, `<XDG_CONFIG_HOME>/stoatty/config.toml`, or `None`
/// when the XDG base directories cannot be resolved.
fn user_config_path() -> Option<PathBuf> {
    Xdg::new()
        .ok()
        .map(|xdg| xdg.config_dir().join("stoatty/config.toml"))
}

/// The user config file's contents, or `None` when it is absent or the config
/// path cannot be resolved. A read failure other than absence is an error.
fn read_user_config() -> Result<Option<String>, ConfigError> {
    let Some(path) = user_config_path() else {
        return Ok(None);
    };

    match std::fs::read_to_string(&path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context(ReadSnafu { path }),
    }
}

/// Overlay `user`'s set fields onto the `default` TOML and deserialize the
/// result into a [`Config`].
///
/// Both are parsed as tables and the user's top-level keys replace the
/// default's, so an absent user field keeps the default. The default ships with
/// the binary and is trusted, so a malformed default panics; a malformed user
/// table is an error.
fn settle(default: &str, user: Option<&str>) -> Result<Config, ConfigError> {
    let mut table: toml::Table =
        toml::from_str(default).expect("embedded default config is valid TOML");

    if let Some(user) = user {
        let user: toml::Table = toml::from_str(user).context(ParseSnafu)?;
        for (key, value) in user {
            table.insert(key, value);
        }
    }

    toml::Value::Table(table)
        .try_into()
        .context(DeserializeSnafu)
}

#[cfg(test)]
mod tests {
    use super::{settle, Config, DEFAULT_CONFIG};

    #[test]
    fn embedded_default_sets_the_doubled_font_size() {
        assert_eq!(
            settle(DEFAULT_CONFIG, None).unwrap(),
            Config { font_size: 30 }
        );
    }

    #[test]
    fn user_file_overrides_set_fields() {
        let config = settle("font_size = 30", Some("font_size = 18")).unwrap();
        assert_eq!(config, Config { font_size: 18 });
    }

    #[test]
    fn absent_user_field_keeps_the_default() {
        let config = settle("font_size = 30", Some("# no overrides here\n")).unwrap();
        assert_eq!(config, Config { font_size: 30 });
    }

    #[test]
    fn malformed_user_config_is_an_error() {
        assert!(settle("font_size = 30", Some("font_size =")).is_err());
    }
}
