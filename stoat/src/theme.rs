//! Runtime theme built from `theme NAME { ... }` blocks in the DSL config.
//!
//! A [`Theme`] is a palette plus a scope → [`Style`] map. Scope lookups
//! use progressive-broadening fallback so tree-sitter captures like
//! `syntax.keyword.control` inherit from `syntax.keyword` when unspecified.

use ratatui::style::{Color, Modifier, Style};
use snafu::{OptionExt, Snafu};
use std::collections::{HashMap, HashSet};
use stoat_config::{Config, Expr, Setting, Spanned, Statement, ThemeBlock, Value};

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    palette: HashMap<String, Color>,
    palette_alpha: HashMap<String, u8>,
    styles: HashMap<String, Style>,
    fg_alpha: HashMap<String, u8>,
    bg_alpha: HashMap<String, u8>,
}

impl Theme {
    /// Empty theme. [`Theme::get`] returns [`Style::default`] for every scope.
    pub fn empty() -> Self {
        Self {
            name: String::new(),
            palette: HashMap::new(),
            palette_alpha: HashMap::new(),
            styles: HashMap::new(),
            fg_alpha: HashMap::new(),
            bg_alpha: HashMap::new(),
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

    /// Alpha (0-255) declared on the scope's foreground via `"#RRGGBBAA"`
    /// or `rgba(r,g,b,a)`. Walks the same progressive-broadening fallback
    /// as [`Self::try_get`]. Returns `None` when no alpha was specified,
    /// which callers should treat as fully opaque.
    pub fn fg_alpha(&self, scope: &str) -> Option<u8> {
        std::iter::successors(Some(scope), |s| Some(s.rsplit_once('.')?.0))
            .find_map(|s| self.fg_alpha.get(s).copied())
    }

    /// Alpha (0-255) declared on the scope's background. See
    /// [`Self::fg_alpha`].
    pub fn bg_alpha(&self, scope: &str) -> Option<u8> {
        std::iter::successors(Some(scope), |s| Some(s.rsplit_once('.')?.0))
            .find_map(|s| self.bg_alpha.get(s).copied())
    }

    pub fn palette_get(&self, name: &str) -> Option<Color> {
        self.palette.get(name).copied()
    }

    /// Alpha (0-255) declared on a palette entry via an 8-digit hex or
    /// `rgba(...)` `let` binding. `None` for plain 6-digit / named entries.
    pub fn palette_alpha(&self, name: &str) -> Option<u8> {
        self.palette_alpha.get(name).copied()
    }

    /// Build a theme from all `theme NAME` blocks in `config` matching `name`.
    /// Multiple blocks of the same name are processed in source order; later
    /// statements override earlier per-field entries, allowing user config
    /// to layer overrides on top of a built-in theme.
    pub fn from_config(config: &Config, name: &str) -> Result<Theme, ThemeError> {
        let blocks: Vec<&Spanned<ThemeBlock>> = config
            .themes
            .iter()
            .filter(|t| t.node.name.node == name)
            .collect();
        if blocks.is_empty() {
            return ThemeNotFoundSnafu {
                name: name.to_string(),
            }
            .fail();
        }

        let mut palette: HashMap<String, (Color, Option<u8>)> = HashMap::new();
        let mut fg: HashMap<String, (Color, Option<u8>)> = HashMap::new();
        let mut bg: HashMap<String, (Color, Option<u8>)> = HashMap::new();
        let mut mods: HashMap<String, Modifier> = HashMap::new();

        for block in &blocks {
            for stmt in &block.node.statements {
                match &stmt.node {
                    Statement::Let(l) => {
                        let entry = resolve_color_from_expr(&l.value.node, &palette)?;
                        palette.insert(l.name.node.clone(), entry);
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
        let mut fg_alpha: HashMap<String, u8> = HashMap::new();
        let mut bg_alpha: HashMap<String, u8> = HashMap::new();
        for scope in scope_set {
            let mut style = Style::default();
            if let Some(&(c, a)) = fg.get(&scope) {
                style = style.fg(c);
                if let Some(a) = a {
                    fg_alpha.insert(scope.clone(), a);
                }
            }
            if let Some(&(c, a)) = bg.get(&scope) {
                style = style.bg(c);
                if let Some(a) = a {
                    bg_alpha.insert(scope.clone(), a);
                }
            }
            if let Some(&m) = mods.get(&scope) {
                if !m.is_empty() {
                    style = style.add_modifier(m);
                }
            }
            styles.insert(scope, style);
        }

        let palette_alpha: HashMap<String, u8> = palette
            .iter()
            .filter_map(|(name, (_, a))| a.map(|a| (name.clone(), a)))
            .collect();
        let palette: HashMap<String, Color> =
            palette.into_iter().map(|(k, (c, _))| (k, c)).collect();

        Ok(Theme {
            name: name.to_string(),
            palette,
            palette_alpha,
            styles,
            fg_alpha,
            bg_alpha,
        })
    }
}

/// Distinct `theme NAME` block names declared in `config`, in source
/// order. A name carrying multiple blocks (user overrides layered on a
/// built-in) appears once, at its first occurrence.
pub fn list_themes(config: &Config) -> Vec<String> {
    let mut seen = HashSet::new();
    config
        .themes
        .iter()
        .filter_map(|block| {
            let name = &block.node.name.node;
            seen.insert(name.clone()).then(|| name.clone())
        })
        .collect()
}

fn apply_setting(
    setting: &Setting,
    palette: &HashMap<String, (Color, Option<u8>)>,
    fg: &mut HashMap<String, (Color, Option<u8>)>,
    bg: &mut HashMap<String, (Color, Option<u8>)>,
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
    palette: &HashMap<String, (Color, Option<u8>)>,
    fg: &mut HashMap<String, (Color, Option<u8>)>,
    bg: &mut HashMap<String, (Color, Option<u8>)>,
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
    palette: &HashMap<String, (Color, Option<u8>)>,
) -> Result<(Color, Option<u8>), ThemeError> {
    match expr {
        Expr::Value(v) => resolve_color_from_value(v, palette),
        Expr::Variable(name) => lookup_named_or_palette(name, palette),
        Expr::If { .. } => UnsupportedExprSnafu.fail(),
        Expr::WithDefault { value, fallback } => {
            match resolve_color_from_expr(&value.node, palette) {
                Ok(c) => Ok(c),
                Err(ThemeError::UnknownColor { .. } | ThemeError::MissingStateRef { .. }) => {
                    resolve_color_from_expr(&fallback.node, palette)
                },
                Err(e) => Err(e),
            }
        },
    }
}

fn resolve_color_from_value(
    value: &Value,
    palette: &HashMap<String, (Color, Option<u8>)>,
) -> Result<(Color, Option<u8>), ThemeError> {
    match value {
        Value::Ident(name) => lookup_named_or_palette(name, palette),
        Value::String(s) => parse_color_string(s),
        Value::StateRef(name) => MissingStateRefSnafu {
            name: name.to_string(),
        }
        .fail(),
        other => InvalidColorValueSnafu {
            value: format!("{other:?}"),
        }
        .fail(),
    }
}

fn lookup_named_or_palette(
    name: &str,
    palette: &HashMap<String, (Color, Option<u8>)>,
) -> Result<(Color, Option<u8>), ThemeError> {
    palette
        .get(name)
        .copied()
        .or_else(|| named_color(name).map(|c| (c, None)))
        .context(UnknownColorSnafu {
            name: name.to_string(),
        })
}

fn parse_color_string(s: &str) -> Result<(Color, Option<u8>), ThemeError> {
    if let Some(hex) = s.strip_prefix('#') {
        let invalid = || InvalidHexColorSnafu {
            value: s.to_string(),
        };
        if hex.len() != 6 && hex.len() != 8 {
            return invalid().fail();
        }
        let r = u8::from_str_radix(&hex[0..2], 16).ok().context(invalid())?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok().context(invalid())?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok().context(invalid())?;
        let alpha = if hex.len() == 8 {
            Some(u8::from_str_radix(&hex[6..8], 16).ok().context(invalid())?)
        } else {
            None
        };
        return Ok((Color::Rgb(r, g, b), alpha));
    }
    if let Some(inner) = s.strip_prefix("ansi(").and_then(|t| t.strip_suffix(')')) {
        let n: u8 = inner.trim().parse().ok().context(InvalidColorValueSnafu {
            value: s.to_string(),
        })?;
        return Ok((Color::Indexed(n), None));
    }
    if let Some(inner) = s.strip_prefix("rgba(").and_then(|t| t.strip_suffix(')')) {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() != 4 {
            return InvalidColorValueSnafu {
                value: s.to_string(),
            }
            .fail();
        }
        let invalid = || InvalidColorValueSnafu {
            value: s.to_string(),
        };
        let r: u8 = parts[0].parse().ok().context(invalid())?;
        let g: u8 = parts[1].parse().ok().context(invalid())?;
        let b: u8 = parts[2].parse().ok().context(invalid())?;
        let a: u8 = parts[3].parse().ok().context(invalid())?;
        return Ok((Color::Rgb(r, g, b), Some(a)));
    }
    named_color(s)
        .context(UnknownColorSnafu {
            name: s.to_string(),
        })
        .map(|c| (c, None))
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
    #[snafu(display("state ref '${name}' has no value in this context"))]
    MissingStateRef {
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
    pub const UI_EDITOR_BACKGROUND: &str = "ui.editor.background";
    pub const UI_EDITOR_ACTIVE_LINE_NUMBER: &str = "ui.editor.active_line_number";
    pub const UI_SURFACE_ELEVATED: &str = "ui.surface.elevated";
    pub const UI_TEXT: &str = "ui.text";
    pub const UI_TEXT_MUTED: &str = "ui.text.muted";
    pub const UI_TEXT_DIM: &str = "ui.text.dim";
    pub const UI_TEXT_DISABLED: &str = "ui.text.disabled";
    pub const UI_TEXT_ACCENT: &str = "ui.text.accent";

    pub const UI_CURSOR: &str = "ui.cursor";
    pub const UI_CURSOR_INPUT: &str = "ui.cursor.input";

    pub const UI_SELECTION: &str = "ui.selection";
    pub const UI_SELECTION_EDITOR: &str = "ui.selection.editor";
    pub const UI_SELECTION_REVERSED: &str = "ui.selection.reversed";

    pub const UI_LINE_HIGHLIGHT: &str = "ui.line_highlight";

    pub const UI_SEARCH_MATCH: &str = "ui.search.match";

    pub const UI_GOTO_WORD_LABEL: &str = "ui.goto_word.label";
    pub const UI_GOTO_WORD_PREFIX: &str = "ui.goto_word.prefix";

    pub const UI_BORDER_FOCUSED: &str = "ui.border.focused";
    pub const UI_BORDER_INACTIVE: &str = "ui.border.inactive";
    pub const UI_BORDER_VARIANT: &str = "ui.border.variant";

    pub const UI_MODAL_HELP: &str = "ui.modal.help";
    pub const UI_MODAL_HINTS: &str = "ui.modal.hints";
    pub const UI_MODAL_PALETTE: &str = "ui.modal.palette";
    pub const UI_MODAL_PICKER: &str = "ui.modal.picker";
    pub const UI_MODAL_SELECTION: &str = "ui.modal.selection";
    pub const UI_MODAL_RUN: &str = "ui.modal.run";

    pub const UI_POPUP_BACKGROUND: &str = "ui.popup.background";
    pub const UI_POPUP_TEXT: &str = "ui.popup.text";
    pub const UI_POPUP_BORDER: &str = "ui.popup.border";
    pub const UI_POPUP_SELECTION_BACKGROUND: &str = "ui.popup.selection.background";
    pub const UI_POPUP_SELECTION_TEXT: &str = "ui.popup.selection.text";

    pub const UI_MINIMAP_THUMB: &str = "ui.minimap.thumb";
    pub const UI_MINIMAP_THUMB_BORDER: &str = "ui.minimap.thumb_border";

    pub const UI_DOCK_MINIMIZED_BACKGROUND: &str = "ui.dock.minimized.background";
    pub const UI_DOCK_MINIMIZED_BORDER: &str = "ui.dock.minimized.border";

    pub const UI_PROMPT: &str = "ui.prompt";
    pub const UI_KEY_LABEL: &str = "ui.key_label";
    pub const UI_HEADING: &str = "ui.heading";
    pub const UI_ERROR: &str = "ui.error";

    pub const UI_BADGE_ACTIVE: &str = "ui.badge.active";
    pub const UI_BADGE_COMPLETE: &str = "ui.badge.complete";
    pub const UI_BADGE_ERROR: &str = "ui.badge.error";

    pub const UI_TAB_ACTIVE: &str = "ui.tab.active";
    pub const UI_TAB_INACTIVE: &str = "ui.tab.inactive";
    pub const UI_TAB_LABEL: &str = "ui.tab.label";

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

    pub const DIFF_STAGED_ADDED: &str = "diff.staged_added";
    pub const DIFF_STAGED_MODIFIED: &str = "diff.staged_modified";
    pub const DIFF_STAGED_DELETED: &str = "diff.staged_deleted";

    pub const DIFF_COMMITTED_ADDED: &str = "diff.committed_added";
    pub const DIFF_COMMITTED_MODIFIED: &str = "diff.committed_modified";
    pub const DIFF_COMMITTED_DELETED: &str = "diff.committed_deleted";

    pub const UI_DIAGNOSTIC_ERROR: &str = "ui.diagnostic.error";
    pub const UI_DIAGNOSTIC_WARNING: &str = "ui.diagnostic.warning";
    pub const UI_DIAGNOSTIC_INFO: &str = "ui.diagnostic.info";
    pub const UI_DIAGNOSTIC_HINT: &str = "ui.diagnostic.hint";

    pub const VCS_GUTTER_ADDED: &str = "vcs.gutter.added";
    pub const VCS_GUTTER_MODIFIED: &str = "vcs.gutter.modified";
    pub const VCS_GUTTER_DELETED: &str = "vcs.gutter.deleted";

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

    pub const UI_DEV_STATS_TEXT: &str = "ui.dev.stats.text";
    pub const UI_DEV_STATS_BACKGROUND: &str = "ui.dev.stats.background";
    pub const UI_DEV_STATS_BORDER: &str = "ui.dev.stats.border";
    pub const UI_DEV_STATS_BAR_GOOD: &str = "ui.dev.stats.bar_good";
    pub const UI_DEV_STATS_BAR_WARN: &str = "ui.dev.stats.bar_warn";
    pub const UI_DEV_STATS_BAR_BAD: &str = "ui.dev.stats.bar_bad";
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
    fn list_themes_returns_distinct_names_in_source_order() {
        let src = r##"
            theme dark { ui.cursor.fg = red; }
            theme light { ui.cursor.fg = blue; }
            theme dark { ui.cursor.bg = green; }
        "##;
        let (config, errors) = parse(src);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let config = config.expect("expected successful parse");
        assert_eq!(
            list_themes(&config),
            vec!["dark".to_string(), "light".to_string()]
        );
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
    fn hex_color_string_with_alpha() {
        let src = r##"theme t {
            ui.selection.editor.bg = "#89b4fa80";
            ui.cursor.fg = "#11223344";
        }"##;
        let theme = load(src, "t");
        assert_eq!(
            theme.try_get("ui.selection.editor").and_then(|s| s.bg),
            Some(Color::Rgb(0x89, 0xb4, 0xfa))
        );
        assert_eq!(theme.bg_alpha("ui.selection.editor"), Some(0x80));
        assert_eq!(theme.fg_alpha("ui.cursor"), Some(0x44));
    }

    #[test]
    fn rgba_color_string() {
        let src = r##"theme t {
            ui.selection.editor.bg = "rgba(137, 180, 250, 128)";
            ui.cursor.fg = "rgba(0,0,0,255)";
        }"##;
        let theme = load(src, "t");
        assert_eq!(
            theme.try_get("ui.selection.editor").and_then(|s| s.bg),
            Some(Color::Rgb(137, 180, 250))
        );
        assert_eq!(theme.bg_alpha("ui.selection.editor"), Some(128));
        assert_eq!(theme.fg_alpha("ui.cursor"), Some(255));
    }

    #[test]
    fn alpha_threads_through_palette_ref() {
        let src = r##"theme t {
            let translucent = "#aabbccdd";
            ui.cursor.bg = translucent;
        }"##;
        let theme = load(src, "t");
        assert_eq!(theme.palette_alpha("translucent"), Some(0xdd));
        assert_eq!(theme.bg_alpha("ui.cursor"), Some(0xdd));
    }

    #[test]
    fn alpha_falls_back_via_progressive_broadening() {
        let src = r##"theme t {
            syntax.keyword.fg = "#11223344";
        }"##;
        let theme = load(src, "t");
        assert_eq!(theme.fg_alpha("syntax.keyword"), Some(0x44));
        assert_eq!(theme.fg_alpha("syntax.keyword.control"), Some(0x44));
        assert_eq!(theme.fg_alpha("syntax.string"), None);
    }

    #[test]
    fn opaque_color_has_no_alpha() {
        let src = r##"theme t {
            ui.cursor.fg = "#89b4fa";
            ui.cursor.bg = red;
        }"##;
        let theme = load(src, "t");
        assert_eq!(theme.fg_alpha("ui.cursor"), None);
        assert_eq!(theme.bg_alpha("ui.cursor"), None);
    }

    #[test]
    fn invalid_hex_alpha_lengths_error() {
        let src7 = r##"theme t { ui.cursor.fg = "#1234567"; }"##;
        assert!(matches!(
            load_err(src7, "t"),
            ThemeError::InvalidHexColor { value, .. } if value == "#1234567"
        ));
        let src9 = r##"theme t { ui.cursor.fg = "#123456789"; }"##;
        assert!(matches!(
            load_err(src9, "t"),
            ThemeError::InvalidHexColor { value, .. } if value == "#123456789"
        ));
    }

    #[test]
    fn invalid_rgba_args_error() {
        let too_few = r##"theme t { ui.cursor.fg = "rgba(1, 2, 3)"; }"##;
        assert!(matches!(
            load_err(too_few, "t"),
            ThemeError::InvalidColorValue { .. }
        ));
        let non_int = r##"theme t { ui.cursor.fg = "rgba(1, 2, 3, x)"; }"##;
        assert!(matches!(
            load_err(non_int, "t"),
            ThemeError::InvalidColorValue { .. }
        ));
        let oversized = r##"theme t { ui.cursor.fg = "rgba(1, 2, 3, 300)"; }"##;
        assert!(matches!(
            load_err(oversized, "t"),
            ThemeError::InvalidColorValue { .. }
        ));
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
    fn with_default_falls_back_on_missing_state_ref() {
        let src = r##"theme t {
            let accent = $missing ?? "#ff0000";
            ui.cursor.fg = accent;
        }"##;
        let theme = load(src, "t");
        assert_eq!(theme.palette_get("accent"), Some(Color::Rgb(0xff, 0, 0)));
        assert_eq!(
            theme.try_get("ui.cursor"),
            Some(Style::default().fg(Color::Rgb(0xff, 0, 0))),
        );
    }

    #[test]
    fn with_default_falls_back_on_unknown_ident() {
        let src = r##"theme t {
            let accent = nosuchcolor ?? "#00ff00";
            ui.cursor.fg = accent;
        }"##;
        let theme = load(src, "t");
        assert_eq!(theme.palette_get("accent"), Some(Color::Rgb(0, 0xff, 0)));
    }

    #[test]
    fn with_default_skips_fallback_when_value_resolves() {
        let src = r##"theme t {
            let primary = "#abcdef";
            let accent = primary ?? "#000000";
            ui.cursor.fg = accent;
        }"##;
        let theme = load(src, "t");
        assert_eq!(
            theme.palette_get("accent"),
            Some(Color::Rgb(0xab, 0xcd, 0xef)),
        );
    }

    #[test]
    fn with_default_chains_walk_to_literal() {
        let src = r##"theme t {
            let accent = $missing_a ?? $missing_b ?? "#0000ff";
            ui.cursor.fg = accent;
        }"##;
        let theme = load(src, "t");
        assert_eq!(theme.palette_get("accent"), Some(Color::Rgb(0, 0, 0xff)));
    }

    #[test]
    fn with_default_propagates_hard_errors() {
        let src = r##"theme t {
            let accent = "#gggggg" ?? "#000000";
            ui.cursor.fg = accent;
        }"##;
        assert!(matches!(
            load_err(src, "t"),
            ThemeError::InvalidHexColor { value, .. } if value == "#gggggg",
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
