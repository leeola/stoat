//! Parse VSCode color-theme JSON into a plain model both stoat and stoatty
//! consume.
//!
//! Marketplace theme files are JSONC -- JSON carrying `//` and `/* */` comments
//! and trailing commas -- so [`parse`] strips those extensions before
//! deserializing. The model stays close to the on-disk shape. Mapping VSCode
//! scopes onto each app's own theme is the caller's job.

use serde::{Deserialize, Deserializer};
use snafu::{ResultExt, Snafu};
use std::collections::BTreeMap;

/// A parsed VSCode color theme.
///
/// `colors` maps workbench color keys (e.g. `editor.background`) to hex strings;
/// `token_colors` holds the TextMate-scoped syntax rules. Every field is
/// optional in the source, so a sparse theme still parses.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct VsCodeTheme {
    #[serde(default)]
    pub name: Option<String>,
    /// The theme's `type` in JSON ("dark" / "light"), under a Rust-legal name.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub colors: BTreeMap<String, String>,
    #[serde(default, rename = "tokenColors")]
    pub token_colors: Vec<TokenRule>,
}

/// One syntax-coloring rule pairing the scopes it targets with the style it
/// assigns.
///
/// Flattens VSCode's nested `{ scope, settings: { foreground, ... } }` shape. A
/// rule's `scope` may be a single string, a comma-separated string, or an array;
/// all three forms land here as separate [`Self::scopes`] entries.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(from = "RawTokenRule")]
pub struct TokenRule {
    pub scopes: Vec<String>,
    pub foreground: Option<String>,
    pub background: Option<String>,
    pub font_style: Option<String>,
}

/// A straight-alpha RGBA color parsed from a CSS-style hex string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// A failure parsing a VSCode theme document.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum ParseError {
    #[snafu(display("invalid theme JSON: {source}"))]
    Json {
        source: serde_json::Error,
        #[snafu(implicit)]
        location: snafu::Location,
    },
}

impl Rgba {
    /// Parse `#rgb`, `#rgba`, `#rrggbb`, or `#rrggbbaa`, case-insensitively.
    ///
    /// Returns [`None`] for a missing `#`, a non-hex digit, or any other length.
    /// Short forms expand each nibble to a byte (`#f00` -> `ff0000`); forms with
    /// no alpha channel default to fully opaque.
    pub fn parse(hex: &str) -> Option<Rgba> {
        let hex = hex.strip_prefix('#')?;
        let bytes = hex.as_bytes();
        let pair = |hi: usize, lo: usize| Some(hex_digit(bytes[hi])? * 16 + hex_digit(bytes[lo])?);
        let single = |i: usize| {
            let v = hex_digit(bytes[i])?;
            Some(v * 16 + v)
        };
        match hex.len() {
            3 => Some(Rgba {
                r: single(0)?,
                g: single(1)?,
                b: single(2)?,
                a: 255,
            }),
            4 => Some(Rgba {
                r: single(0)?,
                g: single(1)?,
                b: single(2)?,
                a: single(3)?,
            }),
            6 => Some(Rgba {
                r: pair(0, 1)?,
                g: pair(2, 3)?,
                b: pair(4, 5)?,
                a: 255,
            }),
            8 => Some(Rgba {
                r: pair(0, 1)?,
                g: pair(2, 3)?,
                b: pair(4, 5)?,
                a: pair(6, 7)?,
            }),
            _ => None,
        }
    }

    /// Composite this color over an opaque `bg` using its alpha, returning the
    /// resulting opaque RGB.
    ///
    /// VSCode themes lean on alpha for selections and overlays. Flattening them
    /// against the background keeps a terminal that only paints opaque cells from
    /// rendering them garishly.
    pub fn blend_over(self, bg: (u8, u8, u8)) -> (u8, u8, u8) {
        let a = self.a as u32;
        let inv = 255 - a;
        let mix = |fg: u8, bg: u8| ((fg as u32 * a + bg as u32 * inv) / 255) as u8;
        (mix(self.r, bg.0), mix(self.g, bg.1), mix(self.b, bg.2))
    }
}

/// Parse a VSCode color-theme document, tolerating JSONC comments and trailing
/// commas.
pub fn parse(source: &str) -> Result<VsCodeTheme, ParseError> {
    let json = strip_jsonc(source);
    serde_json::from_str(&json).context(JsonSnafu)
}

#[derive(Deserialize)]
struct RawTokenRule {
    #[serde(default, deserialize_with = "deserialize_scope")]
    scope: Vec<String>,
    #[serde(default)]
    settings: RawSettings,
}

#[derive(Default, Deserialize)]
struct RawSettings {
    foreground: Option<String>,
    background: Option<String>,
    #[serde(rename = "fontStyle")]
    font_style: Option<String>,
}

impl From<RawTokenRule> for TokenRule {
    fn from(raw: RawTokenRule) -> Self {
        TokenRule {
            scopes: raw.scope,
            foreground: raw.settings.foreground,
            background: raw.settings.background,
            font_style: raw.settings.font_style,
        }
    }
}

/// Accept a scope as a single string, a comma-separated string, or an array of
/// strings, yielding one trimmed entry per scope.
fn deserialize_scope<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Scope {
        One(String),
        Many(Vec<String>),
    }

    Ok(match Scope::deserialize(deserializer)? {
        Scope::One(s) => s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        Scope::Many(v) => v,
    })
}

/// Strip JSONC extensions -- `//` and `/* */` comments and trailing commas -- so
/// a marketplace theme file parses as plain JSON.
///
/// Comment markers and commas inside string literals are preserved. The scan
/// tracks string state and steps over escaped characters.
fn strip_jsonc(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    let mut in_string = false;

    while i < chars.len() {
        let c = chars[i];

        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        match c {
            '"' => {
                in_string = true;
                out.push(c);
                i += 1;
            },
            '/' if i + 1 < chars.len() && chars[i + 1] == '/' => {
                i += 2;
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            },
            '/' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            },
            ',' => {
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                    i += 1;
                } else {
                    out.push(c);
                    i += 1;
                }
            },
            _ => {
                out.push(c);
                i += 1;
            },
        }
    }

    out
}

/// The value of a single hex digit, or [`None`] if `b` is not `0-9a-fA-F`.
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonc_with_comments_and_trailing_commas() {
        let src = r##"{
            // leading line comment
            "name": "Test",
            "type": "dark",
            "colors": {
                "editor.background": "#282c34", /* inline block */
                "editor.foreground": "#abb2bf",
            },
            "tokenColors": [
                { "scope": "keyword", "settings": { "foreground": "#61afef", "fontStyle": "bold" } },
            ],
        }"##;

        let theme = parse(src).expect("jsonc parses");
        assert_eq!(theme.name.as_deref(), Some("Test"));
        assert_eq!(theme.kind.as_deref(), Some("dark"));
        assert_eq!(
            theme.colors.get("editor.background").map(String::as_str),
            Some("#282c34")
        );
        assert_eq!(
            theme.token_colors,
            vec![TokenRule {
                scopes: vec!["keyword".to_string()],
                foreground: Some("#61afef".to_string()),
                background: None,
                font_style: Some("bold".to_string()),
            }]
        );
    }

    #[test]
    fn comma_inside_a_string_is_not_a_trailing_comma() {
        let theme = parse(r#"{ "name": "a, b" }"#).expect("string comma parses");
        assert_eq!(theme.name.as_deref(), Some("a, b"));
    }

    #[test]
    fn scope_accepts_string_array_and_comma_forms() {
        let scopes = |rule: &str| -> Vec<String> {
            let src = format!(r#"{{ "tokenColors": [ {rule} ] }}"#);
            parse(&src).expect("parses").token_colors.remove(0).scopes
        };
        assert_eq!(scopes(r#"{ "scope": "keyword" }"#), vec!["keyword"]);
        assert_eq!(
            scopes(r#"{ "scope": ["keyword", "storage"] }"#),
            vec!["keyword", "storage"]
        );
        assert_eq!(
            scopes(r#"{ "scope": "keyword, storage.type" }"#),
            vec!["keyword", "storage.type"]
        );
    }

    #[test]
    fn parses_hex_forms_case_insensitive() {
        assert_eq!(
            Rgba::parse("#abc"),
            Some(Rgba {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
                a: 255
            })
        );
        assert_eq!(
            Rgba::parse("#abcd"),
            Some(Rgba {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
                a: 0xdd
            })
        );
        assert_eq!(
            Rgba::parse("#AABBCC"),
            Some(Rgba {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
                a: 255
            })
        );
        assert_eq!(
            Rgba::parse("#aAbBcCdD"),
            Some(Rgba {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
                a: 0xdd
            })
        );
    }

    #[test]
    fn rejects_malformed_hex() {
        assert_eq!(Rgba::parse("abc"), None);
        assert_eq!(Rgba::parse("#gg0000"), None);
        assert_eq!(Rgba::parse("#12345"), None);
        assert_eq!(Rgba::parse("#"), None);
    }

    #[test]
    fn blend_over_composites_alpha() {
        let over_black = |c: Rgba| c.blend_over((0, 0, 0));
        assert_eq!(
            over_black(Rgba {
                r: 10,
                g: 20,
                b: 30,
                a: 255
            }),
            (10, 20, 30)
        );
        assert_eq!(
            Rgba {
                r: 10,
                g: 20,
                b: 30,
                a: 0
            }
            .blend_over((100, 110, 120)),
            (100, 110, 120)
        );
        assert_eq!(
            over_black(Rgba {
                r: 200,
                g: 200,
                b: 200,
                a: 128
            }),
            (100, 100, 100)
        );
    }
}
