//! Runtime theme built from `theme NAME { ... }` blocks in the DSL config.
//!
//! A [`Theme`] is a palette plus a scope → [`Style`] map. Scope lookups
//! use progressive-broadening fallback so tree-sitter captures like
//! `syntax.keyword.control` inherit from `syntax.keyword` when unspecified.

use ratatui::style::{Color, Modifier, Style};
use snafu::{OptionExt, Snafu};
use std::collections::HashMap;
use stoat_config::{Config, Expr, Setting, Spanned, Statement, ThemeBlock, Value};

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    palette: HashMap<String, Color>,
    styles: HashMap<String, Style>,
}

impl Theme {
    /// Empty theme. [`Theme::get`] returns [`Style::default`] for every scope.
    pub fn empty() -> Self {
        Self {
            name: String::new(),
            palette: HashMap::new(),
            styles: HashMap::new(),
        }
    }

    /// Look up a scope with progressive-broadening fallback: exact match,
    /// then parent (`a.b.c` → `a.b` → `a`), then [`Style::default`].
    pub fn get(&self, scope: &str) -> Style {
        self.try_get(scope).unwrap_or_default()
    }

    pub fn try_get(&self, scope: &str) -> Option<Style> {
        std::iter::successors(Some(scope), |s| Some(s.rsplit_once('.')?.0))
            .find_map(|s| self.styles.get(s).copied())
    }

    pub fn palette_get(&self, name: &str) -> Option<Color> {
        self.palette.get(name).copied()
    }

    /// Build a theme from every `theme NAME` block in `config` matching `name`.
    ///
    /// A thin wrapper over [`Self::from_blocks`] that filters `config.themes`
    /// by name. See it for the layering contract.
    pub fn from_config(config: &Config, name: &str) -> Result<Theme, ThemeError> {
        let blocks: Vec<&Spanned<ThemeBlock>> = config
            .themes
            .iter()
            .filter(|t| t.node.name.node == name)
            .collect();
        Self::from_blocks(name, &blocks)
    }

    /// Build a theme from `theme NAME` blocks that all carry `name`, already
    /// filtered out of one or more [`Config`]s.
    ///
    /// Blocks are processed in slice order and later statements override
    /// earlier per-field entries, so a built-in theme's blocks followed by a
    /// user's layer the user's overrides field-by-field over the base without
    /// restating the whole theme. Fails when `blocks` is empty -- the named
    /// theme is defined nowhere.
    pub fn from_blocks(name: &str, blocks: &[&Spanned<ThemeBlock>]) -> Result<Theme, ThemeError> {
        if blocks.is_empty() {
            return ThemeNotFoundSnafu {
                name: name.to_string(),
            }
            .fail();
        }

        let mut palette: HashMap<String, Color> = HashMap::new();
        let mut fg: HashMap<String, Color> = HashMap::new();
        let mut bg: HashMap<String, Color> = HashMap::new();
        let mut mods: HashMap<String, Modifier> = HashMap::new();

        for block in blocks {
            for stmt in &block.node.statements {
                match &stmt.node {
                    Statement::Let(l) => {
                        let color = resolve_color_from_expr(&l.value.node, &palette)?;
                        palette.insert(l.name.node.clone(), color);
                    },
                    Statement::Setting(s) => {
                        apply_setting(s, &palette, &mut fg, &mut bg, &mut mods)?;
                    },
                    _ => {},
                }
            }
        }

        let mut scope_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        scope_set.extend(fg.keys().cloned());
        scope_set.extend(bg.keys().cloned());
        scope_set.extend(mods.keys().cloned());

        let mut styles = HashMap::new();
        for scope in scope_set {
            let mut style = Style::default();
            if let Some(&c) = fg.get(&scope) {
                style = style.fg(c);
            }
            if let Some(&c) = bg.get(&scope) {
                style = style.bg(c);
            }
            if let Some(&m) = mods.get(&scope)
                && !m.is_empty()
            {
                style = style.add_modifier(m);
            }
            styles.insert(scope, style);
        }

        Ok(Theme {
            name: name.to_string(),
            palette,
            styles,
        })
    }
}

fn apply_setting(
    setting: &Setting,
    palette: &HashMap<String, Color>,
    fg: &mut HashMap<String, Color>,
    bg: &mut HashMap<String, Color>,
    mods: &mut HashMap<String, Modifier>,
) -> Result<(), ThemeError> {
    let path: Vec<&str> = setting.path.iter().map(|p| p.node.as_str()).collect();
    if let Value::Map(entries) = &setting.value.node {
        let scope = path.join(".");
        for (key, value) in entries {
            apply_field(
                &scope,
                key.node.as_str(),
                &value.node,
                palette,
                fg,
                bg,
                mods,
            )?;
        }
        return Ok(());
    }

    let Some((field, scope_parts)) = path.split_last() else {
        return InvalidScopePathSnafu {
            path: String::new(),
        }
        .fail();
    };
    if scope_parts.is_empty() {
        return InvalidScopePathSnafu {
            path: path.join("."),
        }
        .fail();
    }
    let scope = scope_parts.join(".");
    apply_field(&scope, field, &setting.value.node, palette, fg, bg, mods)
}

fn apply_field(
    scope: &str,
    field: &str,
    value: &Value,
    palette: &HashMap<String, Color>,
    fg: &mut HashMap<String, Color>,
    bg: &mut HashMap<String, Color>,
    mods: &mut HashMap<String, Modifier>,
) -> Result<(), ThemeError> {
    match field {
        "fg" => {
            fg.insert(scope.to_string(), resolve_color_from_value(value, palette)?);
        },
        "bg" => {
            bg.insert(scope.to_string(), resolve_color_from_value(value, palette)?);
        },
        "modifiers" => {
            mods.insert(scope.to_string(), resolve_modifiers(value)?);
        },
        other => {
            return UnknownFieldSnafu {
                scope: scope.to_string(),
                field: other.to_string(),
            }
            .fail();
        },
    }
    Ok(())
}

fn resolve_color_from_expr(
    expr: &Expr,
    palette: &HashMap<String, Color>,
) -> Result<Color, ThemeError> {
    match expr {
        Expr::Value(v) => resolve_color_from_value(v, palette),
        Expr::Variable(name) => lookup_named_or_palette(name, palette),
        Expr::If { .. } => UnsupportedExprSnafu.fail(),
    }
}

fn resolve_color_from_value(
    value: &Value,
    palette: &HashMap<String, Color>,
) -> Result<Color, ThemeError> {
    match value {
        Value::Ident(name) => lookup_named_or_palette(name, palette),
        Value::String(s) => parse_color_string(s),
        other => InvalidColorValueSnafu {
            value: format!("{other:?}"),
        }
        .fail(),
    }
}

fn lookup_named_or_palette(
    name: &str,
    palette: &HashMap<String, Color>,
) -> Result<Color, ThemeError> {
    palette
        .get(name)
        .copied()
        .or_else(|| named_color(name))
        .context(UnknownColorSnafu {
            name: name.to_string(),
        })
}

fn parse_color_string(s: &str) -> Result<Color, ThemeError> {
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() != 6 {
            return InvalidHexColorSnafu {
                value: s.to_string(),
            }
            .fail();
        }
        let r = u8::from_str_radix(&hex[0..2], 16)
            .ok()
            .context(InvalidHexColorSnafu {
                value: s.to_string(),
            })?;
        let g = u8::from_str_radix(&hex[2..4], 16)
            .ok()
            .context(InvalidHexColorSnafu {
                value: s.to_string(),
            })?;
        let b = u8::from_str_radix(&hex[4..6], 16)
            .ok()
            .context(InvalidHexColorSnafu {
                value: s.to_string(),
            })?;
        return Ok(Color::Rgb(r, g, b));
    }
    if let Some(inner) = s.strip_prefix("ansi(").and_then(|t| t.strip_suffix(')')) {
        let n: u8 = inner.trim().parse().ok().context(InvalidColorValueSnafu {
            value: s.to_string(),
        })?;
        return Ok(Color::Indexed(n));
    }
    named_color(s).context(UnknownColorSnafu {
        name: s.to_string(),
    })
}

fn named_color(s: &str) -> Option<Color> {
    let norm = s.replace(['_', '-'], "").to_lowercase();
    Some(match norm.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        "reset" => Color::Reset,
        _ => return None,
    })
}

fn resolve_modifiers(value: &Value) -> Result<Modifier, ThemeError> {
    match value {
        Value::Array(items) => {
            let mut m = Modifier::empty();
            for item in items {
                match &item.node {
                    Value::Ident(name) => m |= named_modifier(name)?,
                    other => {
                        return InvalidModifierSnafu {
                            value: format!("{other:?}"),
                        }
                        .fail();
                    },
                }
            }
            Ok(m)
        },
        other => InvalidModifierSnafu {
            value: format!("{other:?}"),
        }
        .fail(),
    }
}

fn named_modifier(s: &str) -> Result<Modifier, ThemeError> {
    let norm = s.replace(['_', '-'], "").to_lowercase();
    Ok(match norm.as_str() {
        "bold" => Modifier::BOLD,
        "italic" => Modifier::ITALIC,
        "underlined" | "underline" => Modifier::UNDERLINED,
        "reversed" | "reverse" => Modifier::REVERSED,
        "dim" => Modifier::DIM,
        "crossedout" | "crossed" | "strikethrough" => Modifier::CROSSED_OUT,
        "slowblink" => Modifier::SLOW_BLINK,
        "rapidblink" => Modifier::RAPID_BLINK,
        "hidden" => Modifier::HIDDEN,
        _ => {
            return UnknownModifierSnafu {
                name: s.to_string(),
            }
            .fail();
        },
    })
}

#[derive(Debug, Clone, PartialEq, Snafu)]
#[snafu(visibility(pub))]
pub enum ThemeError {
    #[snafu(display("theme '{name}' not found in config"))]
    ThemeNotFound {
        name: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },
    #[snafu(display("unknown palette ref or named color: '{name}'"))]
    UnknownColor {
        name: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },
    #[snafu(display("invalid hex color: '{value}'"))]
    InvalidHexColor {
        value: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },
    #[snafu(display("invalid color value: {value}"))]
    InvalidColorValue {
        value: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },
    #[snafu(display("invalid scope path: '{path}'"))]
    InvalidScopePath {
        path: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },
    #[snafu(display("unknown theme field '{field}' at scope '{scope}'"))]
    UnknownField {
        scope: String,
        field: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },
    #[snafu(display("unknown modifier: '{name}'"))]
    UnknownModifier {
        name: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },
    #[snafu(display("invalid modifier value: {value}"))]
    InvalidModifier {
        value: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },
    #[snafu(display("if-expressions are not supported inside theme blocks"))]
    UnsupportedExpr {
        #[snafu(implicit)]
        location: snafu::Location,
    },
}

/// Typed constants for every UI scope. Call sites use these instead of
/// string literals so typos become compile errors. Syntax scopes
/// (`syntax.*`) stay free-form to accommodate open-ended tree-sitter
/// capture names with fallback.
pub mod scope {
    pub const UI_BACKGROUND: &str = "ui.background";
    pub const UI_TEXT: &str = "ui.text";
    pub const UI_TEXT_MUTED: &str = "ui.text.muted";
    pub const UI_TEXT_DIM: &str = "ui.text.dim";
    pub const UI_TEXT_DISABLED: &str = "ui.text.disabled";

    pub const UI_CURSOR: &str = "ui.cursor";
    pub const UI_CURSOR_INPUT: &str = "ui.cursor.input";

    pub const UI_SELECTION: &str = "ui.selection";
    pub const UI_SELECTION_EDITOR: &str = "ui.selection.editor";
    pub const UI_SELECTION_REVERSED: &str = "ui.selection.reversed";

    pub const UI_SEARCH_MATCH: &str = "ui.search.match";

    pub const UI_HIGHLIGHT_READ: &str = "ui.highlight.read";
    pub const UI_HIGHLIGHT_WRITE: &str = "ui.highlight.write";

    pub const UI_BORDER_FOCUSED: &str = "ui.border.focused";
    pub const UI_BORDER_INACTIVE: &str = "ui.border.inactive";

    pub const UI_MODAL_HELP: &str = "ui.modal.help";
    pub const UI_MODAL_HINTS: &str = "ui.modal.hints";
    pub const UI_MODAL_PALETTE: &str = "ui.modal.palette";
    pub const UI_MODAL_PICKER: &str = "ui.modal.picker";
    pub const UI_MODAL_RUN: &str = "ui.modal.run";

    pub const UI_PROMPT: &str = "ui.prompt";
    pub const UI_KEY_LABEL: &str = "ui.key_label";
    pub const UI_HEADING: &str = "ui.heading";
    pub const UI_ERROR: &str = "ui.error";
    pub const UI_MESSAGE_ERROR: &str = "ui.message.error";

    pub const UI_BADGE_ACTIVE: &str = "ui.badge.active";
    pub const UI_BADGE_COMPLETE: &str = "ui.badge.complete";
    pub const UI_BADGE_ERROR: &str = "ui.badge.error";

    pub const UI_STATUSBAR_FOCUSED: &str = "ui.statusbar.focused";
    pub const UI_STATUSBAR_UNFOCUSED: &str = "ui.statusbar.unfocused";
    pub const UI_MODE_LABEL: &str = "ui.mode_label";

    pub const UI_STATUSLINE_NORMAL: &str = "ui.statusline.normal";
    pub const UI_STATUSLINE_INSERT: &str = "ui.statusline.insert";
    pub const UI_STATUSLINE_SELECT: &str = "ui.statusline.select";
    pub const UI_STATUSLINE_PROMPT: &str = "ui.statusline.prompt";
    pub const UI_STATUSLINE_RUN: &str = "ui.statusline.run";
    pub const UI_STATUSLINE_COMMITS: &str = "ui.statusline.commits";
    pub const UI_STATUSLINE_REBASE: &str = "ui.statusline.rebase";
    pub const UI_STATUSLINE_REWORD: &str = "ui.statusline.reword";
    pub const UI_STATUSLINE_CONFLICT: &str = "ui.statusline.conflict";
    pub const UI_STATUSLINE_REVIEW: &str = "ui.statusline.review";
    pub const UI_STATUSLINE_SUBMODE: &str = "ui.statusline.submode";
    pub const UI_STATUSLINE_DEFAULT: &str = "ui.statusline.default";

    pub const DIFF_ADDED: &str = "diff.added";
    pub const DIFF_DELETED: &str = "diff.deleted";
    pub const DIFF_MODIFIED: &str = "diff.modified";
    pub const DIFF_MOVED: &str = "diff.moved";
    pub const DIFF_CONTEXT: &str = "diff.context";
    pub const DIFF_CURRENT_HUNK: &str = "diff.current_hunk";

    pub const UI_DIAGNOSTIC_ERROR: &str = "ui.diagnostic.error";
    pub const UI_DIAGNOSTIC_WARNING: &str = "ui.diagnostic.warning";
    pub const UI_DIAGNOSTIC_INFO: &str = "ui.diagnostic.info";
    pub const UI_DIAGNOSTIC_HINT: &str = "ui.diagnostic.hint";

    pub const VCS_CONFLICT_HEADER: &str = "vcs.conflict.header";
    pub const VCS_CONFLICT_OURS: &str = "vcs.conflict.ours";
    pub const VCS_CONFLICT_THEIRS: &str = "vcs.conflict.theirs";

    pub const VCS_COMMIT_SHA: &str = "vcs.commit.sha";
    pub const VCS_COMMIT_SUMMARY: &str = "vcs.commit.summary";
    pub const VCS_COMMIT_METADATA: &str = "vcs.commit.metadata";

    pub const VCS_REBASE_PICK: &str = "vcs.rebase.pick";
    pub const VCS_REBASE_SQUASH: &str = "vcs.rebase.squash";
    pub const VCS_REBASE_FIXUP: &str = "vcs.rebase.fixup";
    pub const VCS_REBASE_REWORD: &str = "vcs.rebase.reword";
    pub const VCS_REBASE_EDIT: &str = "vcs.rebase.edit";
    pub const VCS_REBASE_DROP: &str = "vcs.rebase.drop";

    pub const CHAT_USER: &str = "chat.user";
    pub const CHAT_TEXT: &str = "chat.text";
    pub const CHAT_META: &str = "chat.meta";
    pub const CHAT_TIME: &str = "chat.time";
    pub const CHAT_THINKING: &str = "chat.thinking";
    pub const CHAT_TOOL_HEADER: &str = "chat.tool.header";
    pub const CHAT_TOOL_BODY: &str = "chat.tool.body";
    pub const CHAT_TOOL_FOCUSED: &str = "chat.tool.focused";
    pub const CHAT_TOOL_STATUS_RUNNING: &str = "chat.tool.status.running";
    pub const CHAT_TOOL_STATUS_DONE: &str = "chat.tool.status.done";
    pub const CHAT_TOOL_STATUS_FAILED: &str = "chat.tool.status.failed";
    pub const CHAT_TOOL_STATUS_CANCELLED: &str = "chat.tool.status.cancelled";
    pub const CHAT_ERROR: &str = "chat.error";
    pub const CHAT_SEPARATOR: &str = "chat.separator";
    pub const CHAT_THROBBER: &str = "chat.throbber";
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_config::parse;

    fn load(source: &str, name: &str) -> Theme {
        let (config, errors) = parse(source);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let config = config.expect("expected successful parse");
        Theme::from_config(&config, name).expect("theme load failed")
    }

    fn load_err(source: &str, name: &str) -> ThemeError {
        let (config, errors) = parse(source);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let config = config.expect("expected successful parse");
        Theme::from_config(&config, name).expect_err("expected theme load error")
    }

    #[test]
    fn missing_theme_errors() {
        let src = "theme dark { ui.cursor.fg = red; }";
        assert!(matches!(
            load_err(src, "light"),
            ThemeError::ThemeNotFound { name, .. } if name == "light"
        ));
    }

    #[test]
    fn named_color_resolution() {
        let src = "theme t { ui.cursor.fg = red; }";
        let theme = load(src, "t");
        assert_eq!(
            theme.try_get("ui.cursor"),
            Some(Style::default().fg(Color::Red))
        );
    }

    #[test]
    fn hex_color_string() {
        let src = r##"theme t { ui.cursor.fg = "#89b4fa"; }"##;
        let theme = load(src, "t");
        assert_eq!(
            theme.try_get("ui.cursor"),
            Some(Style::default().fg(Color::Rgb(0x89, 0xb4, 0xfa)))
        );
    }

    #[test]
    fn palette_ref_from_let() {
        let src = r##"theme t {
            let accent = "#89b4fa";
            ui.cursor.fg = accent;
        }"##;
        let theme = load(src, "t");
        assert_eq!(
            theme.palette_get("accent"),
            Some(Color::Rgb(0x89, 0xb4, 0xfa))
        );
        assert_eq!(
            theme.try_get("ui.cursor"),
            Some(Style::default().fg(Color::Rgb(0x89, 0xb4, 0xfa)))
        );
    }

    #[test]
    fn map_value_sets_fg_bg_modifiers() {
        let src = r##"theme t {
            let bg = "#000000";
            let accent = "#89b4fa";
            ui.cursor = { fg: bg, bg: accent, modifiers: [bold, italic] };
        }"##;
        let theme = load(src, "t");
        let style = theme.try_get("ui.cursor").unwrap();
        assert_eq!(style.fg, Some(Color::Rgb(0, 0, 0)));
        assert_eq!(style.bg, Some(Color::Rgb(0x89, 0xb4, 0xfa)));
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn dotted_path_form_equivalent_to_map() {
        let map_src = r##"theme t {
            ui.cursor = { fg: red, bg: blue };
        }"##;
        let dotted_src = r##"theme t {
            ui.cursor.fg = red;
            ui.cursor.bg = blue;
        }"##;
        assert_eq!(
            load(map_src, "t").try_get("ui.cursor"),
            load(dotted_src, "t").try_get("ui.cursor")
        );
    }

    #[test]
    fn scope_fallback_broadens_progressively() {
        let src = "theme t { syntax.keyword.fg = red; }";
        let theme = load(src, "t");
        let expected = Some(Style::default().fg(Color::Red));
        assert_eq!(theme.try_get("syntax.keyword"), expected);
        assert_eq!(theme.try_get("syntax.keyword.control"), expected);
        assert_eq!(
            theme.try_get("syntax.keyword.control.conditional"),
            expected
        );
        assert_eq!(theme.try_get("syntax.string"), None);
    }

    #[test]
    fn fallback_prefers_most_specific() {
        let src = r##"theme t {
            syntax.keyword.fg = blue;
            syntax.keyword.control.fg = red;
        }"##;
        let theme = load(src, "t");
        assert_eq!(
            theme.try_get("syntax.keyword"),
            Some(Style::default().fg(Color::Blue))
        );
        assert_eq!(
            theme.try_get("syntax.keyword.control"),
            Some(Style::default().fg(Color::Red))
        );
        assert_eq!(
            theme.try_get("syntax.keyword.control.conditional"),
            Some(Style::default().fg(Color::Red))
        );
    }

    #[test]
    fn unknown_color_ident_errors() {
        let src = "theme t { ui.cursor.fg = notacolor; }";
        assert!(matches!(
            load_err(src, "t"),
            ThemeError::UnknownColor { name, .. } if name == "notacolor"
        ));
    }

    #[test]
    fn invalid_hex_errors() {
        let src = r##"theme t { ui.cursor.fg = "#gggggg"; }"##;
        assert!(matches!(
            load_err(src, "t"),
            ThemeError::InvalidHexColor { value, .. } if value == "#gggggg"
        ));
    }

    #[test]
    fn unknown_field_errors() {
        let src = "theme t { ui.cursor = { color: red }; }";
        assert!(matches!(
            load_err(src, "t"),
            ThemeError::UnknownField { scope, field, .. }
                if scope == "ui.cursor" && field == "color"
        ));
    }

    #[test]
    fn unknown_modifier_errors() {
        let src = "theme t { ui.cursor = { fg: red, modifiers: [sparkles] }; }";
        assert!(matches!(
            load_err(src, "t"),
            ThemeError::UnknownModifier { name, .. } if name == "sparkles"
        ));
    }

    #[test]
    fn later_theme_block_overrides_earlier() {
        let src = r##"
            theme t {
                let accent = "#000000";
                ui.cursor = { fg: accent };
            }
            theme t {
                let accent = "#ffffff";
                ui.cursor.fg = accent;
            }
        "##;
        let theme = load(src, "t");
        assert_eq!(
            theme.palette_get("accent"),
            Some(Color::Rgb(0xff, 0xff, 0xff))
        );
        assert_eq!(
            theme.try_get("ui.cursor"),
            Some(Style::default().fg(Color::Rgb(0xff, 0xff, 0xff)))
        );
    }

    #[test]
    fn partial_override_preserves_untouched_fields() {
        let src = r##"
            theme t {
                ui.cursor = { fg: red, bg: blue };
            }
            theme t {
                ui.cursor.bg = green;
            }
        "##;
        let theme = load(src, "t");
        let style = theme.try_get("ui.cursor").unwrap();
        assert_eq!(style.fg, Some(Color::Red));
        assert_eq!(style.bg, Some(Color::Green));
    }

    #[test]
    fn empty_theme_get_returns_default() {
        let theme = Theme::empty();
        assert_eq!(theme.get("ui.cursor"), Style::default());
        assert_eq!(theme.try_get("ui.cursor"), None);
    }

    #[test]
    fn named_color_case_insensitive_and_aliases() {
        let src = r##"theme t {
            a.fg = gray;
            b.fg = grey;
            c.fg = DarkGray;
            d.fg = light_red;
        }"##;
        let theme = load(src, "t");
        assert_eq!(theme.try_get("a").unwrap().fg, Some(Color::Gray));
        assert_eq!(theme.try_get("b").unwrap().fg, Some(Color::Gray));
        assert_eq!(theme.try_get("c").unwrap().fg, Some(Color::DarkGray));
        assert_eq!(theme.try_get("d").unwrap().fg, Some(Color::LightRed));
    }

    #[test]
    fn ansi_indexed_color() {
        let src = r##"theme t { ui.cursor.fg = "ansi(42)"; }"##;
        let theme = load(src, "t");
        assert_eq!(
            theme.try_get("ui.cursor").unwrap().fg,
            Some(Color::Indexed(42))
        );
    }
}
