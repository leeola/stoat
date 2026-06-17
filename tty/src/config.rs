//! TOML configuration for stoatty.
//!
//! [`load`] settles the embedded default (the repo-root `stoatty.toml`) under
//! the user's file at `<XDG_CONFIG_HOME>/stoatty/config.toml`: a missing user
//! file leaves the defaults in place, and a present one overrides only the
//! fields it sets.

use etcetera::{base_strategy::Xdg, BaseStrategy};
use serde::{de::Error as _, Deserialize, Deserializer};
use snafu::{ResultExt, Snafu};
use std::{collections::BTreeMap, io, path::PathBuf};
use stoatty_term::{grid::Rgb, theme::Theme};

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

    /// Name of the [`themes`](Self::themes) entry colors resolve against.
    #[serde(default)]
    pub theme: String,

    /// Named color themes, keyed by the name [`theme`](Self::theme) selects.
    #[serde(default)]
    pub themes: BTreeMap<String, ThemeColors>,
}

impl Config {
    /// The selected theme resolved into a [`Theme`].
    ///
    /// Starts from [`Theme::default`] and overlays the colors the selected
    /// `[themes.<name>]` entry sets, so an unset color keeps the built-in
    /// default and an unknown selector name yields the default theme.
    pub fn resolve_theme(&self) -> Theme {
        let mut theme = Theme::default();

        let Some(colors) = self.themes.get(&self.theme) else {
            return theme;
        };

        if let Some(HexColor(rgb)) = colors.foreground {
            theme.foreground = rgb;
        }
        if let Some(HexColor(rgb)) = colors.background {
            theme.background = rgb;
        }
        if let Some(HexColor(rgb)) = colors.cursor {
            theme.cursor = rgb;
        }

        let ansi = [
            colors.black,
            colors.red,
            colors.green,
            colors.yellow,
            colors.blue,
            colors.magenta,
            colors.cyan,
            colors.white,
            colors.bright_black,
            colors.bright_red,
            colors.bright_green,
            colors.bright_yellow,
            colors.bright_blue,
            colors.bright_magenta,
            colors.bright_cyan,
            colors.bright_white,
        ];
        for (slot, over) in theme.ansi.iter_mut().zip(ansi) {
            if let Some(HexColor(rgb)) = over {
                *slot = rgb;
            }
        }

        theme
    }
}

/// A named theme's colors as written in the config.
///
/// Each color is an optional `#rrggbb` hex string; an absent one keeps the
/// corresponding [`Theme`] default when [`Config::resolve_theme`] overlays it.
/// The 16 ANSI names map to palette indices 0-7 (normal) and 8-15 (bright).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ThemeColors {
    pub foreground: Option<HexColor>,
    pub background: Option<HexColor>,
    pub cursor: Option<HexColor>,
    pub black: Option<HexColor>,
    pub red: Option<HexColor>,
    pub green: Option<HexColor>,
    pub yellow: Option<HexColor>,
    pub blue: Option<HexColor>,
    pub magenta: Option<HexColor>,
    pub cyan: Option<HexColor>,
    pub white: Option<HexColor>,
    pub bright_black: Option<HexColor>,
    pub bright_red: Option<HexColor>,
    pub bright_green: Option<HexColor>,
    pub bright_yellow: Option<HexColor>,
    pub bright_blue: Option<HexColor>,
    pub bright_magenta: Option<HexColor>,
    pub bright_cyan: Option<HexColor>,
    pub bright_white: Option<HexColor>,
}

/// An `#rrggbb` color from the config, parsed to an [`Rgb`] on deserialize.
///
/// A string that is not a six-digit hex color with a leading `#` is a
/// deserialize error rather than a silently dropped value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HexColor(pub Rgb);

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D>(deserializer: D) -> Result<HexColor, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        parse_hex(&raw)
            .map(HexColor)
            .ok_or_else(|| D::Error::custom(format!("invalid hex color: {raw}")))
    }
}

/// Parse a `#rrggbb` hex color, or `None` if it is malformed.
fn parse_hex(raw: &str) -> Option<Rgb> {
    let digits = raw.strip_prefix('#')?;
    if digits.len() != 6 {
        return None;
    }

    let r = u8::from_str_radix(&digits[0..2], 16).ok()?;
    let g = u8::from_str_radix(&digits[2..4], 16).ok()?;
    let b = u8::from_str_radix(&digits[4..6], 16).ok()?;
    Some(Rgb::new(r, g, b))
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
/// The two are parsed as tables and deep-merged: a key that is a table in both
/// is merged field by field, any other user value replaces the default's, and an
/// absent user key keeps the default. The default ships with the binary and is
/// trusted, so a malformed default panics; a malformed user table is an error.
fn settle(default: &str, user: Option<&str>) -> Result<Config, ConfigError> {
    let mut table: toml::Table =
        toml::from_str(default).expect("embedded default config is valid TOML");

    if let Some(user) = user {
        let user: toml::Table = toml::from_str(user).context(ParseSnafu)?;
        merge_tables(&mut table, user);
    }

    toml::Value::Table(table)
        .try_into()
        .context(DeserializeSnafu)
}

/// Recursively overlay `overlay` onto `base`.
///
/// A key that is a table on both sides is merged field by field, so a user
/// table augments the default rather than replacing it wholesale. Any other
/// value, including a table replacing a non-table or vice versa, takes the
/// overlay's value.
fn merge_tables(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match value {
            toml::Value::Table(sub) => {
                if let Some(toml::Value::Table(base_sub)) = base.get_mut(&key) {
                    merge_tables(base_sub, sub);
                } else {
                    base.insert(key, toml::Value::Table(sub));
                }
            },
            other => {
                base.insert(key, other);
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{merge_tables, settle, DEFAULT_CONFIG};
    use stoatty_term::grid::Rgb;

    #[test]
    fn embedded_default_sets_the_doubled_font_size() {
        assert_eq!(settle(DEFAULT_CONFIG, None).unwrap().font_size, 30);
    }

    #[test]
    fn user_file_overrides_set_fields() {
        let config = settle("font_size = 30", Some("font_size = 18")).unwrap();
        assert_eq!(config.font_size, 18);
    }

    #[test]
    fn absent_user_field_keeps_the_default() {
        let config = settle("font_size = 30", Some("# no overrides here\n")).unwrap();
        assert_eq!(config.font_size, 30);
    }

    #[test]
    fn malformed_user_config_is_an_error() {
        assert!(settle("font_size = 30", Some("font_size =")).is_err());
    }

    #[test]
    fn nested_table_overlay_merges_field_by_field() {
        let mut base: toml::Table =
            toml::from_str("[themes.zed]\nbg = \"black\"\nfg = \"white\"\n").unwrap();
        let overlay: toml::Table =
            toml::from_str("[themes.zed]\nbg = \"navy\"\n[themes.mine]\nbg = \"red\"\n").unwrap();

        merge_tables(&mut base, overlay);

        let expected: toml::Table = toml::from_str(
            "[themes.zed]\nbg = \"navy\"\nfg = \"white\"\n[themes.mine]\nbg = \"red\"\n",
        )
        .unwrap();
        assert_eq!(base, expected);
    }

    #[test]
    fn zed_theme_resolves_to_one_dark_colors() {
        let theme = settle(DEFAULT_CONFIG, None).unwrap().resolve_theme();

        assert_eq!(theme.background, Rgb::new(0x28, 0x2c, 0x34));
        assert_eq!(theme.foreground, Rgb::new(0xab, 0xb2, 0xbf));
        assert_eq!(theme.cursor, Rgb::new(0x74, 0xad, 0xe8));
        assert_eq!(theme.ansi[1], Rgb::new(0xe0, 0x6c, 0x75), "ansi red");
        assert_eq!(
            theme.ansi[15],
            Rgb::new(0xfa, 0xfa, 0xfa),
            "ansi bright white"
        );
    }

    #[test]
    fn user_override_replaces_one_theme_field() {
        let config = settle(
            DEFAULT_CONFIG,
            Some("[themes.zed]\nbackground = \"#000000\"\n"),
        )
        .unwrap();
        let theme = config.resolve_theme();

        assert_eq!(theme.background, Rgb::new(0, 0, 0), "overridden field");
        assert_eq!(theme.foreground, Rgb::new(0xab, 0xb2, 0xbf), "sibling kept");
    }

    #[test]
    fn malformed_theme_hex_is_an_error() {
        assert!(settle(
            DEFAULT_CONFIG,
            Some("[themes.zed]\nbackground = \"nope\"\n")
        )
        .is_err());
    }
}
