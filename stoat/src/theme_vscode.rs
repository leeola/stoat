//! Convert a parsed VSCode color theme into a stoat theme block.
//!
//! The block inherits `default_dark` so scopes the import cannot express (chat,
//! vcs, statusline) still resolve, and it overrides default_dark's palette lets
//! so those inherited scopes recolor to the imported palette. Statements are
//! built as config AST directly rather than DSL source, because DSL idents
//! reject the hyphenated names theme files carry.

use stoat_config::{Expr, LetBinding, Setting, Spanned, Statement, ThemeBlock, Value};
use vscode_theme::{Rgba, TokenRule, VsCodeTheme};

/// Build the theme block named `name` from a parsed VSCode theme.
///
/// The result inherits `default_dark`, overriding only the scopes the theme
/// expresses. Every emitted color is an opaque `#rrggbb`, alpha composited over
/// the theme's editor background. A color that fails to parse is skipped and its
/// scope inherits.
pub(crate) fn theme_block(name: &str, theme: &VsCodeTheme) -> Spanned<ThemeBlock> {
    let background = editor_background(theme);
    let mut statements = Vec::new();
    palette_lets(theme, background, &mut statements);
    ui_settings(theme, background, &mut statements);
    syntax_settings(theme, background, &mut statements);
    spanned(ThemeBlock {
        name: spanned(name.to_string()),
        parent: Some(spanned("default_dark".to_string())),
        statements,
    })
}

/// Override default_dark's base and role lets from the terminal palette (when
/// present) and the always-present editor colors, so inherited scopes recolor.
fn palette_lets(theme: &VsCodeTheme, background: (u8, u8, u8), out: &mut Vec<Spanned<Statement>>) {
    const BASE: &[(&str, &str)] = &[
        ("red", "terminal.ansiRed"),
        ("green", "terminal.ansiGreen"),
        ("yellow", "terminal.ansiYellow"),
        ("blue", "terminal.ansiBlue"),
        ("magenta", "terminal.ansiMagenta"),
        ("cyan", "terminal.ansiCyan"),
        ("light_red", "terminal.ansiBrightRed"),
        ("light_green", "terminal.ansiBrightGreen"),
        ("light_yellow", "terminal.ansiBrightYellow"),
        ("light_blue", "terminal.ansiBrightBlue"),
        ("light_magenta", "terminal.ansiBrightMagenta"),
        ("light_cyan", "terminal.ansiBrightCyan"),
        ("white", "terminal.ansiBrightWhite"),
        ("muted", "terminal.ansiBrightBlack"),
    ];
    const ROLES: &[(&str, &str)] = &[
        ("accent", "cyan"),
        ("info", "cyan"),
        ("primary", "blue"),
        ("success", "green"),
        ("warning", "yellow"),
        ("danger", "red"),
        ("special", "magenta"),
    ];

    if theme.colors.contains_key("terminal.ansiRed") {
        for (name, key) in BASE {
            if let Some(hex) = composited_color(theme, key, background) {
                out.push(let_hex(name, hex));
            }
        }
        // Re-bind the roles so they resolve against the child's base lets above
        // rather than the parent's already-resolved colors.
        for (role, base) in ROLES {
            out.push(let_ref(role, base));
        }
    }

    out.push(let_hex("black", rgb_hex(background)));
    if let Some(foreground) = editor_foreground(theme) {
        out.push(let_hex("subtle", rgb_hex(lighten(background, foreground))));
        let fg_hex = rgb_hex(foreground);
        out.push(let_hex("text", fg_hex.clone()));
        out.push(let_hex("dim", fg_hex.clone()));
        out.push(let_hex("gray", fg_hex));
    }
}

/// Emit `ui.*` settings from the workbench colors the theme provides.
fn ui_settings(theme: &VsCodeTheme, background: (u8, u8, u8), out: &mut Vec<Spanned<Statement>>) {
    const TABLE: &[(&str, &[&str])] = &[
        ("editor.background", &["ui", "background", "bg"]),
        ("editor.foreground", &["ui", "text", "fg"]),
        (
            "editor.selectionBackground",
            &["ui", "selection", "editor", "bg"],
        ),
        ("focusBorder", &["ui", "border", "focused", "fg"]),
        (
            "statusBar.background",
            &["ui", "statusbar", "focused", "bg"],
        ),
        (
            "statusBar.foreground",
            &["ui", "statusbar", "focused", "fg"],
        ),
    ];
    for (key, path) in TABLE {
        if let Some(hex) = composited_color(theme, key, background) {
            out.push(scalar_setting(path, hex));
        }
    }
}

/// Emit `syntax.*` settings from the token rules, matching each stoat scope to
/// the rule with the most specific scope prefix.
fn syntax_settings(
    theme: &VsCodeTheme,
    background: (u8, u8, u8),
    out: &mut Vec<Spanned<Statement>>,
) {
    const TABLE: &[(&str, &[&str])] = &[
        ("keyword", &["syntax", "keyword"]),
        ("string", &["syntax", "string"]),
        ("constant.character.escape", &["syntax", "string", "escape"]),
        ("comment", &["syntax", "comment"]),
        (
            "comment.block.documentation",
            &["syntax", "comment", "documentation"],
        ),
        ("entity.name.function", &["syntax", "function"]),
        ("entity.name.type", &["syntax", "type"]),
        ("constant", &["syntax", "constant"]),
        ("keyword.operator", &["syntax", "operator"]),
        ("punctuation", &["syntax", "punctuation"]),
        ("variable.other.member", &["syntax", "property"]),
        ("entity.other.attribute-name", &["syntax", "attribute"]),
        ("variable", &["syntax", "variable"]),
        ("markup.heading", &["syntax", "markup", "title"]),
        ("markup.underline.link", &["syntax", "markup", "link_uri"]),
    ];
    for (wanted, scope) in TABLE {
        let Some(rule) = best_rule(theme, wanted) else {
            continue;
        };
        let Some(hex) = rule
            .foreground
            .as_deref()
            .and_then(|hex| composite(hex, background))
        else {
            continue;
        };
        let modifiers = font_style_modifiers(rule.font_style.as_deref());
        if modifiers.is_empty() {
            let mut path = scope.to_vec();
            path.push("fg");
            out.push(scalar_setting(&path, hex));
        } else {
            out.push(map_setting(scope, hex, modifiers));
        }
    }
}

/// The token rule whose scope is the longest dot-boundary prefix of `wanted`, so
/// a specific rule (`keyword.operator`) wins over a general one (`keyword`).
fn best_rule<'a>(theme: &'a VsCodeTheme, wanted: &str) -> Option<&'a TokenRule> {
    theme
        .token_colors
        .iter()
        .filter_map(|rule| {
            rule.scopes
                .iter()
                .filter(|scope| scope_prefixes(scope, wanted))
                .map(String::len)
                .max()
                .map(|len| (len, rule))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, rule)| rule)
}

/// Whether `scope` equals `wanted` or is a dot-boundary prefix of it.
fn scope_prefixes(scope: &str, wanted: &str) -> bool {
    wanted == scope || wanted.starts_with(&format!("{scope}."))
}

/// Translate a VSCode `fontStyle` string into stoat modifier idents, dropping
/// words stoat has no modifier for.
fn font_style_modifiers(font_style: Option<&str>) -> Vec<Spanned<Value>> {
    font_style
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|word| match word {
            "italic" => Some("italic"),
            "bold" => Some("bold"),
            "underline" => Some("underlined"),
            "strikethrough" => Some("strikethrough"),
            _ => None,
        })
        .map(|modifier| spanned(Value::Ident(modifier.to_string())))
        .collect()
}

fn editor_background(theme: &VsCodeTheme) -> (u8, u8, u8) {
    theme
        .colors
        .get("editor.background")
        .and_then(|hex| Rgba::parse(hex))
        .map(|c| (c.r, c.g, c.b))
        .unwrap_or((0, 0, 0))
}

fn editor_foreground(theme: &VsCodeTheme) -> Option<(u8, u8, u8)> {
    theme
        .colors
        .get("editor.foreground")
        .and_then(|hex| Rgba::parse(hex))
        .map(|c| (c.r, c.g, c.b))
}

/// Parse `theme.colors[key]` and composite its alpha over `background`.
fn composited_color(theme: &VsCodeTheme, key: &str, background: (u8, u8, u8)) -> Option<String> {
    composite(theme.colors.get(key)?, background)
}

/// Parse `hex` and composite its alpha over `background`, as an opaque `#rrggbb`.
fn composite(hex: &str, background: (u8, u8, u8)) -> Option<String> {
    Some(rgb_hex(Rgba::parse(hex)?.blend_over(background)))
}

/// Lighten `background` about 8% toward `foreground` for the subtle highlight.
fn lighten(background: (u8, u8, u8), foreground: (u8, u8, u8)) -> (u8, u8, u8) {
    Rgba {
        r: foreground.0,
        g: foreground.1,
        b: foreground.2,
        a: 20,
    }
    .blend_over(background)
}

fn rgb_hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn let_hex(name: &str, hex: String) -> Spanned<Statement> {
    spanned(Statement::Let(LetBinding {
        name: spanned(name.to_string()),
        value: spanned(Expr::Value(Value::String(hex))),
    }))
}

fn let_ref(name: &str, target: &str) -> Spanned<Statement> {
    spanned(Statement::Let(LetBinding {
        name: spanned(name.to_string()),
        value: spanned(Expr::Value(Value::Ident(target.to_string()))),
    }))
}

fn scalar_setting(path: &[&str], hex: String) -> Spanned<Statement> {
    spanned(Statement::Setting(Setting {
        path: path.iter().map(|s| spanned(s.to_string())).collect(),
        value: spanned(Value::String(hex)),
    }))
}

fn map_setting(scope: &[&str], hex: String, modifiers: Vec<Spanned<Value>>) -> Spanned<Statement> {
    spanned(Statement::Setting(Setting {
        path: scope.iter().map(|s| spanned(s.to_string())).collect(),
        value: spanned(Value::Map(vec![
            (spanned("fg".to_string()), spanned(Value::String(hex))),
            (
                spanned("modifiers".to_string()),
                spanned(Value::Array(modifiers)),
            ),
        ])),
    }))
}

fn spanned<T>(node: T) -> Spanned<T> {
    Spanned::new(node, 0..0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use ratatui::style::{Color, Modifier};
    use vscode_theme::parse;

    fn resolve(name: &str, theme: &VsCodeTheme) -> Theme {
        let (config, _) = stoat_config::parse(crate::app::DEFAULT_KEYMAP);
        let config = config.expect("default config parses");
        let block = theme_block(name, theme);
        let mut pool: Vec<&Spanned<ThemeBlock>> = config.themes.iter().collect();
        pool.push(&block);
        Theme::from_blocks(name, &pool).expect("theme resolves")
    }

    #[test]
    fn imported_syntax_colors_apply_with_specific_scope_winning() {
        let vt = parse(
            r##"{
                "colors": { "editor.background": "#101010", "editor.foreground": "#e0e0e0" },
                "tokenColors": [
                    { "scope": "keyword", "settings": { "foreground": "#ff8800", "fontStyle": "bold" } },
                    { "scope": "keyword.operator", "settings": { "foreground": "#00ff88" } }
                ]
            }"##,
        )
        .expect("fixture parses");

        let theme = resolve("import", &vt);
        let keyword = theme.get("syntax.keyword");
        assert_eq!(keyword.fg, Some(Color::Rgb(0xff, 0x88, 0x00)));
        assert!(keyword.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            theme.get("syntax.operator").fg,
            Some(Color::Rgb(0x00, 0xff, 0x88))
        );
    }

    #[test]
    fn uncovered_scope_inherits_default_dark_recolored() {
        let vt = parse(
            r##"{
                "colors": {
                    "editor.background": "#101010",
                    "editor.foreground": "#e0e0e0",
                    "terminal.ansiRed": "#aa0000",
                    "terminal.ansiGreen": "#00aa00",
                    "terminal.ansiBlue": "#0000aa",
                    "terminal.ansiCyan": "#00aaaa"
                }
            }"##,
        )
        .expect("fixture parses");

        // default_dark assigns `chat.user.fg = success` and `success = green`. The
        // converter has no chat scope, so the color must come from the recolored let.
        let theme = resolve("recolor", &vt);
        assert_eq!(
            theme.get("chat.user").fg,
            Some(Color::Rgb(0x00, 0xaa, 0x00))
        );
    }

    #[test]
    fn alpha_selection_blends_over_background() {
        let vt = parse(
            r##"{ "colors": { "editor.background": "#000000", "editor.selectionBackground": "#ffffff80" } }"##,
        )
        .expect("fixture parses");

        let theme = resolve("alpha", &vt);
        assert_eq!(
            theme.get("ui.selection.editor").bg,
            Some(Color::Rgb(128, 128, 128))
        );
    }

    #[test]
    fn terminal_less_theme_still_recolors_ui_background() {
        let vt =
            parse(r##"{ "colors": { "editor.background": "#123456" } }"##).expect("fixture parses");
        let theme = resolve("noterm", &vt);
        assert_eq!(
            theme.get("ui.background").bg,
            Some(Color::Rgb(0x12, 0x34, 0x56))
        );
    }
}
