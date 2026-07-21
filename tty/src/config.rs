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
use vscode_theme::{Rgba, VsCodeTheme};

/// The default configuration, embedded from the repo root so a built binary
/// carries it without the source tree.
const DEFAULT_CONFIG: &str = include_str!("../../stoatty.toml");

/// The built-in VSCode themes, embedded so `theme = "one-dark"` and
/// `theme = "gruvbox-dark"` resolve without any file on disk.
const THEME_ONE_DARK: &str = include_str!("../../themes/one-dark.json");
const THEME_GRUVBOX_DARK: &str = include_str!("../../themes/gruvbox-dark.json");

/// The `terminal.ansi*` color keys in palette-index order, the 8 normal colors
/// followed by the 8 bright ones.
const ANSI_KEYS: [&str; 16] = [
    "terminal.ansiBlack",
    "terminal.ansiRed",
    "terminal.ansiGreen",
    "terminal.ansiYellow",
    "terminal.ansiBlue",
    "terminal.ansiMagenta",
    "terminal.ansiCyan",
    "terminal.ansiWhite",
    "terminal.ansiBrightBlack",
    "terminal.ansiBrightRed",
    "terminal.ansiBrightGreen",
    "terminal.ansiBrightYellow",
    "terminal.ansiBrightBlue",
    "terminal.ansiBrightMagenta",
    "terminal.ansiBrightCyan",
    "terminal.ansiBrightWhite",
];

/// The settled stoatty configuration.
///
/// Every field is optional in a user file; an omitted field keeps the embedded
/// default's value.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Config {
    /// Font size in logical points; the renderer scales it by the display's
    /// scale factor to get the physical rasterization size.
    #[serde(default)]
    pub font_size: u32,

    /// Ordered cascade of font family names, most preferred first. The renderer
    /// shapes text with the first family it finds installed, so later entries
    /// are fallbacks for when an earlier one is missing.
    #[serde(default)]
    pub font_family: Vec<String>,

    /// Name of the [`themes`](Self::themes) entry colors resolve against.
    #[serde(default)]
    pub theme: String,

    /// Whether the renderer shapes contiguous same-style cell runs together so
    /// the font's ligatures form across cells. When off, each cell is shaped on
    /// its own, so no ligatures appear.
    #[serde(default)]
    pub ligatures: bool,

    /// Selected cursor motion style. The default `block` is today's rigid
    /// square. `warp` stretches the cursor along its path between cells.
    #[serde(default = "default_cursor_animation")]
    pub cursor_animation: CursorAnimation,

    /// Program to launch over the PTY instead of the default stoat editor, with
    /// its arguments. `None`, the default, launches the resolved stoat.
    #[serde(default)]
    pub shell: Option<ShellConfig>,

    /// Path to the stoat editor binary launched as the default child. `None`,
    /// the default, resolves stoat at runtime by sibling lookup. The `STOAT_BIN`
    /// environment variable overrides this.
    #[serde(default)]
    pub stoat_program: Option<PathBuf>,

    /// Named color themes, keyed by the name [`theme`](Self::theme) selects.
    #[serde(default)]
    pub themes: BTreeMap<String, ThemeColors>,

    /// VSCode color themes available to [`theme`](Self::theme), keyed by file
    /// stem.
    ///
    /// These come from the built-in themes and the JSON files dropped in the
    /// shared themes dir, not from the config file, so [`load`] fills the field
    /// after deserializing rather than reading it out of the TOML. A
    /// [`themes`](Self::themes) entry of the same name shadows one of these.
    #[serde(skip)]
    pub vscode_themes: BTreeMap<String, VsCodeTheme>,
}

impl Config {
    /// The selected theme resolved into a [`Theme`].
    ///
    /// A `[themes.<name>]` entry wins, so a hand-written TOML block always
    /// overrides a VSCode theme of the same name. Failing that the name is
    /// looked up among the [`vscode_themes`](Self::vscode_themes). A name
    /// matching neither yields [`Theme::default`].
    pub fn resolve_theme(&self) -> Theme {
        if let Some(colors) = self.themes.get(&self.theme) {
            return theme_from_toml(colors);
        }
        match self.vscode_themes.get(&self.theme) {
            Some(source) => theme_from_vscode(source),
            None => Theme::default(),
        }
    }
}

/// Overlay a `[themes.<name>]` entry's colors onto [`Theme::default`].
///
/// An unset color keeps the built-in default rather than blanking the slot.
fn theme_from_toml(colors: &ThemeColors) -> Theme {
    let mut theme = Theme::default();

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

/// Derive a terminal [`Theme`] from a VSCode color theme.
///
/// A VSCode theme colors an editor, not a terminal, so only its `terminal.*`
/// keys map directly. Where those are absent the editor's own colors stand in,
/// which is what makes a theme written with no integrated-terminal section
/// still usable here. Any slot the theme leaves unspecified keeps its
/// [`Theme::default`] value rather than going black.
///
/// Themes lean on alpha for overlays, so every color is composited down to an
/// opaque one over the resolved background.
fn theme_from_vscode(source: &VsCodeTheme) -> Theme {
    let fallback = Theme::default();
    let color = |key: &str| source.colors.get(key).and_then(|hex| Rgba::parse(hex));
    let first = |keys: [&str; 2]| keys.into_iter().find_map(color);

    let background = match first(["terminal.background", "editor.background"]) {
        Some(rgba) => flatten(rgba, fallback.background),
        None => fallback.background,
    };
    let foreground = first(["terminal.foreground", "editor.foreground"])
        .map(|rgba| flatten(rgba, background))
        .unwrap_or(fallback.foreground);
    let cursor = first(["terminalCursor.foreground", "editorCursor.foreground"])
        .map(|rgba| flatten(rgba, background))
        .unwrap_or(foreground);

    let mut ansi = fallback.ansi;
    for (slot, key) in ansi.iter_mut().zip(ANSI_KEYS) {
        if let Some(rgba) = color(key) {
            *slot = flatten(rgba, background);
        }
    }

    Theme {
        foreground,
        background,
        cursor,
        ansi,
    }
}

/// Composite `rgba` over the opaque `bg`, yielding an opaque [`Rgb`].
fn flatten(rgba: Rgba, bg: Rgb) -> Rgb {
    let (r, g, b) = rgba.blend_over((bg.r, bg.g, bg.b));
    Rgb::new(r, g, b)
}

/// Cursor motion style selectable in the config.
///
/// [`Block`](Self::Block) keeps today's rigid square that slides between cells.
/// [`Warp`](Self::Warp) stretches the cursor along the path from its old cell to
/// its new one, Neovide-style, then snaps back to a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorAnimation {
    #[default]
    Block,
    Warp,
}

/// The cursor animation used when the config omits the key, for the
/// `serde(default)` on [`Config::cursor_animation`].
fn default_cursor_animation() -> CursorAnimation {
    CursorAnimation::Block
}

/// A program to launch over the PTY instead of the default shell, with its
/// arguments.
///
/// Set by a `[shell]` table in the config; absent leaves [`Config::shell`] as
/// `None`. `args` is empty when the table omits it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ShellConfig {
    /// Path or name of the program to launch over the PTY.
    pub program: String,

    /// Arguments passed to [`program`](Self::program).
    #[serde(default)]
    pub args: Vec<String>,
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
    let mut config = settle(DEFAULT_CONFIG, read_user_config()?.as_deref())?;
    config.vscode_themes = parse_vscode_themes(vscode_theme_sources());
    Ok(config)
}

/// The embedded default configuration, with no user overrides applied.
///
/// The fallback when [`load`] fails. It carries the shipped defaults, a font
/// size of 15 and the `one-dark` theme, rather than zeroed fields, so a window
/// opened after a bad user config still renders with the intended look. The
/// embedded default ships with the binary and is trusted, so a malformed one
/// panics.
pub fn embedded_default() -> Config {
    let mut config = settle(DEFAULT_CONFIG, None).expect("embedded default config settles");
    config.vscode_themes = parse_vscode_themes(vscode_theme_sources());
    config
}

/// The user config path, `<XDG_CONFIG_HOME>/stoatty/config.toml`, or `None`
/// when the XDG base directories cannot be resolved.
fn user_config_path() -> Option<PathBuf> {
    Xdg::new()
        .ok()
        .map(|xdg| xdg.config_dir().join("stoatty/config.toml"))
}

/// The themes directory, `<XDG_CONFIG_HOME>/stoat/themes`, or `None` when the
/// XDG base directories cannot be resolved.
///
/// This is deliberately stoat's directory rather than stoatty's. One drop point
/// serves both programs, so a theme placed there is selectable in the terminal
/// and in the editor it hosts.
fn user_themes_dir() -> Option<PathBuf> {
    Xdg::new()
        .ok()
        .map(|xdg| xdg.config_dir().join("stoat/themes"))
}

/// The `(stem, JSON)` pairs of every available VSCode theme, built-ins first.
///
/// User files come last so one whose stem matches a built-in shadows it.
fn vscode_theme_sources() -> Vec<(String, String)> {
    let mut sources = builtin_vscode_themes();
    sources.extend(user_vscode_themes());
    sources
}

/// The `(stem, JSON)` pairs of the themes embedded in the binary.
fn builtin_vscode_themes() -> Vec<(String, String)> {
    vec![
        ("one-dark".to_string(), THEME_ONE_DARK.to_string()),
        ("gruvbox-dark".to_string(), THEME_GRUVBOX_DARK.to_string()),
    ]
}

/// The `(stem, JSON)` pairs of the `*.json` files in the themes directory.
///
/// An unresolvable or unreadable directory yields no themes rather than an
/// error, since a user with no themes of their own is the common case.
fn user_vscode_themes() -> Vec<(String, String)> {
    let Some(dir) = user_themes_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()? != "json" {
                return None;
            }
            let stem = path.file_stem()?.to_str()?.to_string();
            Some((stem, std::fs::read_to_string(&path).ok()?))
        })
        .collect()
}

/// Parse theme sources into a map keyed by stem, dropping the ones that fail.
///
/// A malformed theme file is logged and skipped rather than failing the load,
/// so one bad file in the themes directory cannot stop the terminal from
/// starting. Later sources win, so the caller orders them by precedence.
fn parse_vscode_themes(sources: Vec<(String, String)>) -> BTreeMap<String, VsCodeTheme> {
    sources
        .into_iter()
        .filter_map(|(stem, source)| match vscode_theme::parse(&source) {
            Ok(theme) => Some((stem, theme)),
            Err(error) => {
                tracing::warn!(%error, theme = %stem, "skipping malformed VSCode theme");
                None
            },
        })
        .collect()
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
    use super::{
        builtin_vscode_themes, embedded_default, merge_tables, parse_vscode_themes, settle, Config,
        CursorAnimation, ShellConfig, DEFAULT_CONFIG,
    };
    use stoatty_term::{grid::Rgb, theme::Theme};

    #[test]
    fn embedded_default_sets_the_logical_font_size() {
        assert_eq!(settle(DEFAULT_CONFIG, None).unwrap().font_size, 15);
    }

    #[test]
    fn embedded_default_carries_the_shipped_config() {
        let config = embedded_default();
        assert_eq!(config.font_size, 15);
        assert_eq!(config.font_family, ["JetBrains Mono", "monospace"]);
        assert_eq!(config.theme, "one-dark");
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
    fn user_shell_override_sets_program_and_args() {
        let config = settle(
            DEFAULT_CONFIG,
            Some("[shell]\nprogram = \"/bin/bash\"\nargs = [\"--login\"]\n"),
        )
        .unwrap();
        assert_eq!(
            config.shell,
            Some(ShellConfig {
                program: "/bin/bash".to_string(),
                args: vec!["--login".to_string()],
            })
        );
    }

    #[test]
    fn absent_shell_defaults_to_none() {
        assert_eq!(settle(DEFAULT_CONFIG, None).unwrap().shell, None);
    }

    #[test]
    fn ligatures_default_on_and_user_can_disable() {
        assert!(
            settle(DEFAULT_CONFIG, None).unwrap().ligatures,
            "the embedded default ships ligatures on"
        );
        assert!(
            !settle(DEFAULT_CONFIG, Some("ligatures = false\n"))
                .unwrap()
                .ligatures,
            "a user file turns ligatures off"
        );
    }

    #[test]
    fn cursor_animation_default_block_and_user_can_warp() {
        assert_eq!(
            settle(DEFAULT_CONFIG, None).unwrap().cursor_animation,
            CursorAnimation::Block,
            "the embedded default ships the block cursor animation"
        );
        assert_eq!(
            settle(DEFAULT_CONFIG, Some("cursor_animation = \"warp\"\n"))
                .unwrap()
                .cursor_animation,
            CursorAnimation::Warp,
            "a user file selects the warp cursor animation"
        );
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
    fn default_theme_resolves_to_one_dark_colors() {
        let mut config = settle(DEFAULT_CONFIG, None).unwrap();
        config.vscode_themes = parse_vscode_themes(builtin_vscode_themes());
        let theme = config.resolve_theme();

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
            Some("theme = \"zed\"\n[themes.zed]\nbackground = \"#000000\"\n"),
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

    /// Settle `user` over the shipped default, then stock the VSCode map from
    /// `sources`, standing in for what [`load`] reads off disk.
    fn config_with_vscode(user: &str, sources: &[(&str, &str)]) -> Config {
        let owned = sources
            .iter()
            .map(|(stem, json)| (stem.to_string(), json.to_string()))
            .collect();

        let mut config = settle(DEFAULT_CONFIG, Some(user)).unwrap();
        config.vscode_themes = parse_vscode_themes(owned);
        config
    }

    #[test]
    fn vscode_theme_resolves_by_name() {
        let json = r##"{ "colors": {
            "terminal.background": "#101010",
            "terminal.foreground": "#e0e0e0",
            "terminalCursor.foreground": "#ff8800",
            "terminal.ansiRed": "#ff0000"
        } }"##;
        let theme = config_with_vscode("theme = \"probe\"", &[("probe", json)]).resolve_theme();

        assert_eq!(theme.background, Rgb::new(0x10, 0x10, 0x10));
        assert_eq!(theme.foreground, Rgb::new(0xe0, 0xe0, 0xe0));
        assert_eq!(theme.cursor, Rgb::new(0xff, 0x88, 0x00));
        assert_eq!(theme.ansi[1], Rgb::new(0xff, 0x00, 0x00), "ansi red");
        assert_eq!(
            theme.ansi[2],
            Theme::default().ansi[2],
            "a slot the theme omits keeps the built-in default"
        );
    }

    #[test]
    fn toml_theme_shadows_a_same_name_vscode_theme() {
        let json = r##"{ "colors": { "terminal.background": "#101010" } }"##;
        let user = "theme = \"probe\"\n[themes.probe]\nbackground = \"#010203\"\n";
        let theme = config_with_vscode(user, &[("probe", json)]).resolve_theme();

        assert_eq!(theme.background, Rgb::new(0x01, 0x02, 0x03));
    }

    #[test]
    fn vscode_theme_falls_back_to_editor_colors() {
        let json = r##"{ "colors": {
            "editor.background": "#202020",
            "editor.foreground": "#c0c0c0"
        } }"##;
        let theme = config_with_vscode("theme = \"probe\"", &[("probe", json)]).resolve_theme();

        assert_eq!(theme.background, Rgb::new(0x20, 0x20, 0x20));
        assert_eq!(theme.foreground, Rgb::new(0xc0, 0xc0, 0xc0));
        assert_eq!(
            theme.cursor,
            Rgb::new(0xc0, 0xc0, 0xc0),
            "a theme naming no cursor color uses its foreground"
        );
    }

    #[test]
    fn vscode_alpha_composites_over_the_background() {
        let json = r##"{ "colors": {
            "terminal.background": "#000000",
            "terminal.ansiBlue": "#ffffff80"
        } }"##;
        let theme = config_with_vscode("theme = \"probe\"", &[("probe", json)]).resolve_theme();

        assert_eq!(
            theme.ansi[4],
            Rgb::new(128, 128, 128),
            "half-transparent white over black lands midway"
        );
    }

    #[test]
    fn builtin_gruvbox_dark_resolves_with_no_user_files() {
        let mut config = settle(DEFAULT_CONFIG, Some("theme = \"gruvbox-dark\"")).unwrap();
        config.vscode_themes = parse_vscode_themes(builtin_vscode_themes());
        let theme = config.resolve_theme();

        assert_eq!(theme.background, Rgb::new(0x28, 0x28, 0x28));
        assert_eq!(theme.foreground, Rgb::new(0xeb, 0xdb, 0xb2));
        assert_eq!(theme.ansi[1], Rgb::new(0xcc, 0x24, 0x1d), "ansi red");
        assert_eq!(
            theme.ansi[15],
            Rgb::new(0xeb, 0xdb, 0xb2),
            "ansi bright white"
        );
    }

    #[test]
    fn a_malformed_theme_file_is_skipped() {
        let themes = parse_vscode_themes(vec![
            ("bad".to_string(), "{ not json".to_string()),
            (
                "good".to_string(),
                r##"{ "colors": { "terminal.background": "#101010" } }"##.to_string(),
            ),
        ]);

        assert_eq!(
            themes.keys().collect::<Vec<_>>(),
            ["good"],
            "one bad file does not take the rest down with it"
        );
    }
}
