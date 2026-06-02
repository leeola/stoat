//! Typed view of stcfg settings, with a merge operator so CLI/env flags
//! can override values loaded from config files.
//!
//! Each field is [`Option`] so "not set" is distinguishable from "set to
//! the default", which is the signal [`Settings::merge`] uses to decide
//! whether an override wins. Consumers read via
//! `settings.field.unwrap_or(default)` at the point of use.

use crate::ast::{Config, EventType, Setting, Statement, Value};
use snafu::{Location, Snafu};
use std::collections::BTreeMap;

/// Default placement for a newly-opened Claude chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudePlacement {
    Pane,
    DockLeft,
    DockRight,
}

/// Mouse-capture policy applied at terminal startup. `Auto` keeps the
/// parent-multiplexer guard (capture disabled when `$TMUX` or `$ZELLIJ`
/// is set so the parent owns drag-select); `Always` forces capture on
/// regardless of nesting; `Never` disables it unconditionally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseCapturePolicy {
    Auto,
    Always,
    Never,
}

/// Gutter line-number display mode. `Absolute` shows the 1-based row
/// number; `Relative` shows each row's distance from the cursor row
/// (the cursor row reads 0); `Hybrid` shows the absolute number on the
/// cursor row and the distance on every other row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineNumberMode {
    Absolute,
    Relative,
    Hybrid,
}

/// Visible-whitespace rendering mode. `None` draws no whitespace
/// glyphs (the default); `Boundary` marks only leading and trailing
/// whitespace; `Selection` marks whitespace inside the active
/// selection; `All` marks every space and tab. A trailing-whitespace
/// underline is drawn regardless of this setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowWhitespace {
    None,
    Boundary,
    Selection,
    All,
}

/// Per-tool Claude permission rule lists. Each `Vec<String>` carries
/// raw regex source as parsed from stcfg; compilation happens at the
/// host's policy construction so a bad pattern can be reported with
/// context rather than failing config load. Set via stcfg paths
/// `claude.permissions.<tool>.always_allow|always_confirm|always_deny =
/// [pattern, ...]`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ToolPermissions {
    pub always_allow: Vec<String>,
    pub always_confirm: Vec<String>,
    pub always_deny: Vec<String>,
}

/// Spawn arguments for a per-language LSP server. The LSP launcher
/// reads the matching entry from
/// [`Settings::language_servers`] (keyed by language name) and uses
/// `command` plus `args` as the child-process invocation, with each
/// `env` pair exported into the child's environment.
///
/// Populated from stcfg paths
/// `lsp.<lang>.command = "..."`,
/// `lsp.<lang>.args = ["...", ...]`,
/// `lsp.<lang>.env.<KEY> = "..."`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LanguageServerCommand {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// Top-level resolved settings struct.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Settings {
    /// Enables the Claude Code / LSP text-protocol transcript log.
    pub text_proto_log: Option<bool>,
    /// Default placement of `OpenClaude`. `None` means "pane".
    pub claude_default_placement: Option<ClaudePlacement>,
    /// Name of the active theme block. Resolves against `theme NAME { ... }`
    /// blocks in the config. `None` means "use the compiled-in default".
    pub theme: Option<String>,
    /// Mouse-capture policy at terminal startup. `None` falls back to
    /// [`MouseCapturePolicy::Auto`].
    pub mouse_capture: Option<MouseCapturePolicy>,
    /// Per-mode status-line badge label overrides, keyed by mode name.
    /// Set via `ui.mode_badge.<name> = "ABC";` in stcfg. Renderer
    /// consults this map before falling back to its hardcoded badge
    /// table; user-defined modes can supply their own entry here so
    /// the status line shows something more meaningful than `---`.
    pub mode_badges: BTreeMap<String, String>,
    /// Per-tool Claude permission rules, keyed by tool name (e.g.
    /// `Bash`, `Read`, `WebFetch`). Empty when no rules are
    /// configured. Right-hand wins on merge: a CLI override fully
    /// replaces the file's rules for any tool it specifies.
    pub claude_permissions: BTreeMap<String, ToolPermissions>,
    /// Monospace font family for the editor pane. Set via
    /// `editor.font.family = "Menlo";`.
    pub editor_font_family: Option<String>,
    /// Editor pane font size in logical pixels. Set via
    /// `editor.font.size = 14;`.
    pub editor_font_size: Option<f32>,
    /// Proportional font family for chrome (status bar, tab bar,
    /// modals, dock panels). Set via `ui.font.family = "SF Pro";`.
    pub ui_font_family: Option<String>,
    /// Chrome font size in logical pixels. Set via
    /// `ui.font.size = 14;`.
    pub ui_font_size: Option<f32>,
    /// Show the per-pane tab bar above editor content. `None` defaults
    /// to visible. Set via `ui.pane.show_tab_bar = false;` to hide.
    pub ui_pane_show_tab_bar: Option<bool>,
    /// Show the per-pane breadcrumbs bar above editor content. `None`
    /// defaults to visible. Set via `ui.pane.show_breadcrumbs = false;`
    /// to hide.
    pub ui_pane_show_breadcrumbs: Option<bool>,
    /// Paint diagnostic, git-hunk, and search markers on the editor
    /// scrollbar. `None` defaults to enabled. Set via
    /// `ui.editor.show_scrollbar_markers = false;` to hide.
    pub ui_editor_show_scrollbar_markers: Option<bool>,
    /// Show inline git blame (author + relative age) at the end of
    /// each editable line. `None` defaults to off. Set via
    /// `ui.editor.show_inline_blame = true;` to enable.
    pub ui_editor_show_inline_blame: Option<bool>,
    /// Show indent guides (vertical lines at tab-stop columns) in the
    /// editor. `None` defaults to on. Set via
    /// `ui.editor.show_indent_guides = false;` to hide.
    pub ui_editor_show_indent_guides: Option<bool>,
    /// Show sticky scroll headers (pin the enclosing container's
    /// signature at the viewport top). `None` defaults to on. Set via
    /// `ui.editor.show_sticky_scroll = false;` to hide.
    pub ui_editor_show_sticky_scroll: Option<bool>,
    /// Gutter line-number display mode. `None` defaults to
    /// [`LineNumberMode::Absolute`]. Set via
    /// `ui.editor.line_numbers = relative;` (or `hybrid`).
    pub ui_editor_line_numbers: Option<LineNumberMode>,
    /// Visible-whitespace rendering mode. `None` defaults to
    /// [`ShowWhitespace::None`] (no glyphs). Set via
    /// `ui.editor.show_whitespace = all;` (or `boundary`/`selection`).
    pub ui_editor_show_whitespace: Option<ShowWhitespace>,
    /// Per-language LSP server commands keyed by language name
    /// (e.g. `rust`, `typescript`). Empty when no
    /// `lsp.<lang>.*` settings are present. Right-hand wins on
    /// merge: an override naming one language replaces that key
    /// entirely without touching others.
    pub language_servers: BTreeMap<String, LanguageServerCommand>,
}

impl Settings {
    /// Extracts known settings from `on init` blocks in `config`. Unknown
    /// setting paths are silently ignored so a config file that references
    /// a future setting on an older binary does not fail to parse.
    pub fn from_config(config: &Config) -> Self {
        let mut out = Settings::default();
        for block in &config.blocks {
            if block.node.event != EventType::Init {
                continue;
            }
            for stmt in &block.node.statements {
                if let Statement::Setting(setting) = &stmt.node {
                    out.apply(setting);
                }
            }
        }
        out
    }

    /// Right-hand wins: any `Some(_)` field in `other` overrides the
    /// corresponding field in `self`. Used to layer CLI over stcfg.
    pub fn merge(self, other: Settings) -> Settings {
        let mut mode_badges = self.mode_badges;
        mode_badges.extend(other.mode_badges);
        let mut claude_permissions = self.claude_permissions;
        claude_permissions.extend(other.claude_permissions);
        let mut language_servers = self.language_servers;
        language_servers.extend(other.language_servers);
        Settings {
            text_proto_log: other.text_proto_log.or(self.text_proto_log),
            claude_default_placement: other
                .claude_default_placement
                .or(self.claude_default_placement),
            theme: other.theme.or(self.theme),
            mouse_capture: other.mouse_capture.or(self.mouse_capture),
            mode_badges,
            claude_permissions,
            editor_font_family: other.editor_font_family.or(self.editor_font_family),
            editor_font_size: other.editor_font_size.or(self.editor_font_size),
            ui_font_family: other.ui_font_family.or(self.ui_font_family),
            ui_font_size: other.ui_font_size.or(self.ui_font_size),
            ui_pane_show_tab_bar: other.ui_pane_show_tab_bar.or(self.ui_pane_show_tab_bar),
            ui_pane_show_breadcrumbs: other
                .ui_pane_show_breadcrumbs
                .or(self.ui_pane_show_breadcrumbs),
            ui_editor_show_scrollbar_markers: other
                .ui_editor_show_scrollbar_markers
                .or(self.ui_editor_show_scrollbar_markers),
            ui_editor_show_inline_blame: other
                .ui_editor_show_inline_blame
                .or(self.ui_editor_show_inline_blame),
            ui_editor_show_indent_guides: other
                .ui_editor_show_indent_guides
                .or(self.ui_editor_show_indent_guides),
            ui_editor_show_sticky_scroll: other
                .ui_editor_show_sticky_scroll
                .or(self.ui_editor_show_sticky_scroll),
            ui_editor_line_numbers: other.ui_editor_line_numbers.or(self.ui_editor_line_numbers),
            ui_editor_show_whitespace: other
                .ui_editor_show_whitespace
                .or(self.ui_editor_show_whitespace),
            language_servers,
        }
    }

    fn apply(&mut self, setting: &Setting) {
        let path: Vec<&str> = setting.path.iter().map(|p| p.node.as_str()).collect();
        match path.as_slice() {
            ["text_proto_log"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.text_proto_log = Some(b);
                }
            },
            ["claude", "default_placement"] => {
                let raw = match &setting.value.node {
                    Value::String(s) | Value::Ident(s) => Some(s.as_str()),
                    _ => None,
                };
                let placement = match raw {
                    Some("pane") => Some(ClaudePlacement::Pane),
                    Some("dock-left") => Some(ClaudePlacement::DockLeft),
                    Some("dock-right") => Some(ClaudePlacement::DockRight),
                    _ => None,
                };
                if let Some(p) = placement {
                    self.claude_default_placement = Some(p);
                }
            },
            ["theme"] => {
                if let Value::Ident(s) | Value::String(s) = &setting.value.node {
                    self.theme = Some(s.clone());
                }
            },
            ["ui", "mode_badge", name] => {
                if let Value::String(badge) | Value::Ident(badge) = &setting.value.node {
                    self.mode_badges.insert((*name).to_string(), badge.clone());
                }
            },
            ["mouse", "capture"] => {
                let raw = match &setting.value.node {
                    Value::String(s) | Value::Ident(s) => Some(s.as_str()),
                    _ => None,
                };
                let policy = match raw {
                    Some("auto") => Some(MouseCapturePolicy::Auto),
                    Some("always") => Some(MouseCapturePolicy::Always),
                    Some("never") => Some(MouseCapturePolicy::Never),
                    _ => None,
                };
                if let Some(p) = policy {
                    self.mouse_capture = Some(p);
                }
            },
            ["editor", "font", "family"] => {
                if let Value::String(s) = &setting.value.node {
                    self.editor_font_family = Some(s.clone());
                }
            },
            ["editor", "font", "size"] => {
                if let Value::Number(n) = setting.value.node {
                    self.editor_font_size = Some(n as f32);
                }
            },
            ["ui", "font", "family"] => {
                if let Value::String(s) = &setting.value.node {
                    self.ui_font_family = Some(s.clone());
                }
            },
            ["ui", "font", "size"] => {
                if let Value::Number(n) = setting.value.node {
                    self.ui_font_size = Some(n as f32);
                }
            },
            ["ui", "pane", "show_tab_bar"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.ui_pane_show_tab_bar = Some(b);
                }
            },
            ["ui", "pane", "show_breadcrumbs"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.ui_pane_show_breadcrumbs = Some(b);
                }
            },
            ["ui", "editor", "show_scrollbar_markers"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.ui_editor_show_scrollbar_markers = Some(b);
                }
            },
            ["ui", "editor", "show_inline_blame"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.ui_editor_show_inline_blame = Some(b);
                }
            },
            ["ui", "editor", "show_indent_guides"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.ui_editor_show_indent_guides = Some(b);
                }
            },
            ["ui", "editor", "show_sticky_scroll"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.ui_editor_show_sticky_scroll = Some(b);
                }
            },
            ["ui", "editor", "line_numbers"] => {
                let mode = match &setting.value.node {
                    Value::String(s) | Value::Ident(s) => parse_line_number_mode(s),
                    _ => None,
                };
                if let Some(mode) = mode {
                    self.ui_editor_line_numbers = Some(mode);
                }
            },
            ["ui", "editor", "show_whitespace"] => {
                let mode = match &setting.value.node {
                    Value::String(s) | Value::Ident(s) => parse_show_whitespace(s),
                    _ => None,
                };
                if let Some(mode) = mode {
                    self.ui_editor_show_whitespace = Some(mode);
                }
            },
            ["claude", "permissions", tool, behavior] => {
                let Value::Array(items) = &setting.value.node else {
                    return;
                };
                let patterns: Vec<String> = items
                    .iter()
                    .filter_map(|item| match &item.node {
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                let entry = self
                    .claude_permissions
                    .entry((*tool).to_string())
                    .or_default();
                match *behavior {
                    "always_allow" => entry.always_allow = patterns,
                    "always_confirm" => entry.always_confirm = patterns,
                    "always_deny" => entry.always_deny = patterns,
                    _ => {},
                }
            },
            ["lsp", lang, "command"] => {
                if let Value::String(s) = &setting.value.node {
                    self.language_servers
                        .entry((*lang).to_string())
                        .or_default()
                        .command = s.clone();
                }
            },
            ["lsp", lang, "args"] => {
                let Value::Array(items) = &setting.value.node else {
                    return;
                };
                let args: Vec<String> = items
                    .iter()
                    .filter_map(|item| match &item.node {
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                self.language_servers
                    .entry((*lang).to_string())
                    .or_default()
                    .args = args;
            },
            ["lsp", lang, "env", key] => {
                if let Value::String(s) = &setting.value.node {
                    self.language_servers
                        .entry((*lang).to_string())
                        .or_default()
                        .env
                        .insert((*key).to_string(), s.clone());
                }
            },
            _ => {},
        }
    }

    /// Apply a `<path> = <raw>` assignment at runtime, parsing the
    /// stringly value the same way the palette's `Set` action would.
    /// The `path` argument is the dotted setting key (e.g.
    /// `ui.pane.show_tab_bar`); `raw` is the unparsed value as the
    /// user typed it.
    pub fn apply_runtime(&mut self, path: &str, raw: &str) -> Result<(), SettingsApplyError> {
        let tokens: Vec<&str> = path.split('.').collect();
        match tokens.as_slice() {
            ["ui", "pane", "show_tab_bar"] => {
                let b = parse_bool(raw).ok_or_else(|| {
                    InvalidValueSnafu {
                        key: path.to_string(),
                        expected: "bool",
                        got: raw.to_string(),
                    }
                    .build()
                })?;
                self.ui_pane_show_tab_bar = Some(b);
                Ok(())
            },
            ["ui", "pane", "show_breadcrumbs"] => {
                let b = parse_bool(raw).ok_or_else(|| {
                    InvalidValueSnafu {
                        key: path.to_string(),
                        expected: "bool",
                        got: raw.to_string(),
                    }
                    .build()
                })?;
                self.ui_pane_show_breadcrumbs = Some(b);
                Ok(())
            },
            ["ui", "editor", "show_scrollbar_markers"] => {
                let b = parse_bool(raw).ok_or_else(|| {
                    InvalidValueSnafu {
                        key: path.to_string(),
                        expected: "bool",
                        got: raw.to_string(),
                    }
                    .build()
                })?;
                self.ui_editor_show_scrollbar_markers = Some(b);
                Ok(())
            },
            ["ui", "editor", "show_inline_blame"] => {
                let b = parse_bool(raw).ok_or_else(|| {
                    InvalidValueSnafu {
                        key: path.to_string(),
                        expected: "bool",
                        got: raw.to_string(),
                    }
                    .build()
                })?;
                self.ui_editor_show_inline_blame = Some(b);
                Ok(())
            },
            ["ui", "editor", "show_indent_guides"] => {
                let b = parse_bool(raw).ok_or_else(|| {
                    InvalidValueSnafu {
                        key: path.to_string(),
                        expected: "bool",
                        got: raw.to_string(),
                    }
                    .build()
                })?;
                self.ui_editor_show_indent_guides = Some(b);
                Ok(())
            },
            ["ui", "editor", "show_sticky_scroll"] => {
                let b = parse_bool(raw).ok_or_else(|| {
                    InvalidValueSnafu {
                        key: path.to_string(),
                        expected: "bool",
                        got: raw.to_string(),
                    }
                    .build()
                })?;
                self.ui_editor_show_sticky_scroll = Some(b);
                Ok(())
            },
            ["ui", "editor", "line_numbers"] => {
                let mode = parse_line_number_mode(raw).ok_or_else(|| {
                    InvalidValueSnafu {
                        key: path.to_string(),
                        expected: "absolute|relative|hybrid",
                        got: raw.to_string(),
                    }
                    .build()
                })?;
                self.ui_editor_line_numbers = Some(mode);
                Ok(())
            },
            ["ui", "editor", "show_whitespace"] => {
                let mode = parse_show_whitespace(raw).ok_or_else(|| {
                    InvalidValueSnafu {
                        key: path.to_string(),
                        expected: "none|boundary|selection|all",
                        got: raw.to_string(),
                    }
                    .build()
                })?;
                self.ui_editor_show_whitespace = Some(mode);
                Ok(())
            },
            _ => UnknownKeySnafu {
                key: path.to_string(),
            }
            .fail(),
        }
    }
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "on" => Some(true),
        "false" | "no" | "0" | "off" => Some(false),
        _ => None,
    }
}

fn parse_line_number_mode(raw: &str) -> Option<LineNumberMode> {
    match raw.to_ascii_lowercase().as_str() {
        "absolute" => Some(LineNumberMode::Absolute),
        "relative" => Some(LineNumberMode::Relative),
        "hybrid" => Some(LineNumberMode::Hybrid),
        _ => None,
    }
}

fn parse_show_whitespace(raw: &str) -> Option<ShowWhitespace> {
    match raw.to_ascii_lowercase().as_str() {
        "none" | "off" => Some(ShowWhitespace::None),
        "boundary" => Some(ShowWhitespace::Boundary),
        "selection" => Some(ShowWhitespace::Selection),
        "all" => Some(ShowWhitespace::All),
        _ => None,
    }
}

/// Failure modes for [`Settings::apply_runtime`]. `UnknownKey` covers
/// dotted paths the runtime dispatcher does not recognize; the typed
/// `apply` path silently ignores unknowns so a forward-compatible
/// stcfg doesn't fail to parse on older binaries, but the runtime
/// path surfaces the error so users get feedback on typos.
#[derive(Debug, PartialEq, Snafu)]
#[snafu(visibility(pub))]
pub enum SettingsApplyError {
    #[snafu(display("unknown setting key `{key}`"))]
    UnknownKey {
        key: String,
        #[snafu(implicit)]
        location: Location,
    },
    #[snafu(display("setting `{key}` expects {expected}, got `{got}`"))]
    InvalidValue {
        key: String,
        expected: &'static str,
        got: String,
        #[snafu(implicit)]
        location: Location,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn parse_ok(source: &str) -> Config {
        let (config, errors) = parse(source);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        config.expect("expected successful parse")
    }

    #[test]
    fn apply_runtime_sets_ui_pane_show_tab_bar_false() {
        let mut s = Settings::default();
        assert_eq!(s.apply_runtime("ui.pane.show_tab_bar", "false"), Ok(()));
        assert_eq!(s.ui_pane_show_tab_bar, Some(false));
    }

    #[test]
    fn apply_runtime_sets_ui_pane_show_tab_bar_true() {
        let mut s = Settings::default();
        assert_eq!(s.apply_runtime("ui.pane.show_tab_bar", "on"), Ok(()));
        assert_eq!(s.ui_pane_show_tab_bar, Some(true));
    }

    #[test]
    fn apply_runtime_sets_ui_pane_show_breadcrumbs_false() {
        let mut s = Settings::default();
        assert_eq!(s.apply_runtime("ui.pane.show_breadcrumbs", "false"), Ok(()));
        assert_eq!(s.ui_pane_show_breadcrumbs, Some(false));
    }

    #[test]
    fn apply_runtime_sets_ui_editor_show_scrollbar_markers_false() {
        let mut s = Settings::default();
        assert_eq!(
            s.apply_runtime("ui.editor.show_scrollbar_markers", "false"),
            Ok(())
        );
        assert_eq!(s.ui_editor_show_scrollbar_markers, Some(false));
    }

    #[test]
    fn apply_runtime_sets_ui_editor_show_inline_blame_true() {
        let mut s = Settings::default();
        assert_eq!(
            s.apply_runtime("ui.editor.show_inline_blame", "true"),
            Ok(())
        );
        assert_eq!(s.ui_editor_show_inline_blame, Some(true));
    }

    #[test]
    fn apply_runtime_sets_ui_editor_show_indent_guides_false() {
        let mut s = Settings::default();
        assert_eq!(
            s.apply_runtime("ui.editor.show_indent_guides", "false"),
            Ok(())
        );
        assert_eq!(s.ui_editor_show_indent_guides, Some(false));
    }

    #[test]
    fn apply_runtime_sets_ui_editor_show_sticky_scroll_false() {
        let mut s = Settings::default();
        assert_eq!(
            s.apply_runtime("ui.editor.show_sticky_scroll", "false"),
            Ok(())
        );
        assert_eq!(s.ui_editor_show_sticky_scroll, Some(false));
    }

    #[test]
    fn apply_runtime_sets_ui_editor_line_numbers_relative() {
        let mut s = Settings::default();
        assert_eq!(
            s.apply_runtime("ui.editor.line_numbers", "relative"),
            Ok(())
        );
        assert_eq!(s.ui_editor_line_numbers, Some(LineNumberMode::Relative));
    }

    #[test]
    fn apply_runtime_rejects_invalid_line_number_mode() {
        let mut s = Settings::default();
        let result = s.apply_runtime("ui.editor.line_numbers", "sideways");
        assert!(matches!(
            result,
            Err(SettingsApplyError::InvalidValue { .. })
        ));
    }

    #[test]
    fn apply_runtime_sets_ui_editor_show_whitespace() {
        for (raw, mode) in [
            ("all", ShowWhitespace::All),
            ("boundary", ShowWhitespace::Boundary),
            ("selection", ShowWhitespace::Selection),
            ("none", ShowWhitespace::None),
        ] {
            let mut s = Settings::default();
            assert_eq!(s.apply_runtime("ui.editor.show_whitespace", raw), Ok(()));
            assert_eq!(s.ui_editor_show_whitespace, Some(mode));
        }
        let mut s = Settings::default();
        assert!(matches!(
            s.apply_runtime("ui.editor.show_whitespace", "sometimes"),
            Err(SettingsApplyError::InvalidValue { .. })
        ));
    }

    #[test]
    fn apply_runtime_rejects_unknown_key() {
        let mut s = Settings::default();
        let result = s.apply_runtime("nope.bad.path", "true");
        assert!(matches!(result, Err(SettingsApplyError::UnknownKey { .. })));
    }

    #[test]
    fn apply_runtime_rejects_invalid_value() {
        let mut s = Settings::default();
        let result = s.apply_runtime("ui.pane.show_tab_bar", "maybe");
        match result {
            Err(SettingsApplyError::InvalidValue {
                key,
                expected,
                got,
                location: _,
            }) => {
                assert_eq!(key, "ui.pane.show_tab_bar");
                assert_eq!(expected, "bool");
                assert_eq!(got, "maybe");
            },
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn from_config_parses_ui_pane_show_tab_bar_false() {
        let config = parse_ok("on init { ui.pane.show_tab_bar = false; }");
        let settings = Settings::from_config(&config);
        assert_eq!(settings.ui_pane_show_tab_bar, Some(false));
    }

    #[test]
    fn from_config_parses_ui_pane_show_tab_bar_true() {
        let config = parse_ok("on init { ui.pane.show_tab_bar = true; }");
        let settings = Settings::from_config(&config);
        assert_eq!(settings.ui_pane_show_tab_bar, Some(true));
    }

    #[test]
    fn from_config_parses_ui_pane_show_breadcrumbs_false() {
        let config = parse_ok("on init { ui.pane.show_breadcrumbs = false; }");
        let settings = Settings::from_config(&config);
        assert_eq!(settings.ui_pane_show_breadcrumbs, Some(false));
    }

    #[test]
    fn from_config_parses_ui_editor_show_scrollbar_markers_false() {
        let config = parse_ok("on init { ui.editor.show_scrollbar_markers = false; }");
        let settings = Settings::from_config(&config);
        assert_eq!(settings.ui_editor_show_scrollbar_markers, Some(false));
    }

    #[test]
    fn from_config_parses_ui_editor_show_inline_blame_true() {
        let config = parse_ok("on init { ui.editor.show_inline_blame = true; }");
        let settings = Settings::from_config(&config);
        assert_eq!(settings.ui_editor_show_inline_blame, Some(true));
    }

    #[test]
    fn from_config_parses_ui_editor_show_indent_guides_false() {
        let config = parse_ok("on init { ui.editor.show_indent_guides = false; }");
        let settings = Settings::from_config(&config);
        assert_eq!(settings.ui_editor_show_indent_guides, Some(false));
    }

    #[test]
    fn from_config_parses_ui_editor_show_sticky_scroll_false() {
        let config = parse_ok("on init { ui.editor.show_sticky_scroll = false; }");
        let settings = Settings::from_config(&config);
        assert_eq!(settings.ui_editor_show_sticky_scroll, Some(false));
    }

    #[test]
    fn from_config_parses_ui_editor_line_numbers_hybrid() {
        let config = parse_ok("on init { ui.editor.line_numbers = hybrid; }");
        let settings = Settings::from_config(&config);
        assert_eq!(
            settings.ui_editor_line_numbers,
            Some(LineNumberMode::Hybrid)
        );
    }

    #[test]
    fn from_config_extracts_text_proto_log() {
        let config = parse_ok("on init { text_proto_log = true; }");
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: Some(true),
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                claude_permissions: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                ui_font_family: None,
                ui_font_size: None,
                ui_pane_show_tab_bar: None,
                ui_pane_show_breadcrumbs: None,
                ui_editor_show_scrollbar_markers: None,
                ui_editor_show_inline_blame: None,
                ui_editor_show_indent_guides: None,
                ui_editor_show_sticky_scroll: None,
                ui_editor_line_numbers: None,
                ui_editor_show_whitespace: None,
                language_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn from_config_false_value() {
        let config = parse_ok("on init { text_proto_log = false; }");
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: Some(false),
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                claude_permissions: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                ui_font_family: None,
                ui_font_size: None,
                ui_pane_show_tab_bar: None,
                ui_pane_show_breadcrumbs: None,
                ui_editor_show_scrollbar_markers: None,
                ui_editor_show_inline_blame: None,
                ui_editor_show_indent_guides: None,
                ui_editor_show_sticky_scroll: None,
                ui_editor_line_numbers: None,
                ui_editor_show_whitespace: None,
                language_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn from_config_ignores_unknown_paths() {
        let config = parse_ok("on init { some.unknown.path = true; text_proto_log = true; }");
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: Some(true),
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                claude_permissions: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                ui_font_family: None,
                ui_font_size: None,
                ui_pane_show_tab_bar: None,
                ui_pane_show_breadcrumbs: None,
                ui_editor_show_scrollbar_markers: None,
                ui_editor_show_inline_blame: None,
                ui_editor_show_indent_guides: None,
                ui_editor_show_sticky_scroll: None,
                ui_editor_line_numbers: None,
                ui_editor_show_whitespace: None,
                language_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn from_config_ignores_non_init_blocks() {
        let config = parse_ok("on key { text_proto_log = true; }");
        assert_eq!(Settings::from_config(&config), Settings::default());
    }

    #[test]
    fn from_config_wrong_value_type_ignored() {
        let config = parse_ok(r#"on init { text_proto_log = "yes"; }"#);
        assert_eq!(Settings::from_config(&config), Settings::default());
    }

    #[test]
    fn merge_right_wins_over_some() {
        let left = Settings {
            text_proto_log: Some(false),
            claude_default_placement: None,
            theme: None,
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            claude_permissions: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            ui_font_family: None,
            ui_font_size: None,
            ui_pane_show_tab_bar: None,
            ui_pane_show_breadcrumbs: None,
            ui_editor_show_scrollbar_markers: None,
            ui_editor_show_inline_blame: None,
            ui_editor_show_indent_guides: None,
            ui_editor_show_sticky_scroll: None,
            ui_editor_line_numbers: None,
            ui_editor_show_whitespace: None,
            language_servers: BTreeMap::new(),
        };
        let right = Settings {
            text_proto_log: Some(true),
            claude_default_placement: None,
            theme: None,
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            claude_permissions: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            ui_font_family: None,
            ui_font_size: None,
            ui_pane_show_tab_bar: None,
            ui_pane_show_breadcrumbs: None,
            ui_editor_show_scrollbar_markers: None,
            ui_editor_show_inline_blame: None,
            ui_editor_show_indent_guides: None,
            ui_editor_show_sticky_scroll: None,
            ui_editor_line_numbers: None,
            ui_editor_show_whitespace: None,
            language_servers: BTreeMap::new(),
        };
        assert_eq!(
            left.merge(right),
            Settings {
                text_proto_log: Some(true),
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                claude_permissions: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                ui_font_family: None,
                ui_font_size: None,
                ui_pane_show_tab_bar: None,
                ui_pane_show_breadcrumbs: None,
                ui_editor_show_scrollbar_markers: None,
                ui_editor_show_inline_blame: None,
                ui_editor_show_indent_guides: None,
                ui_editor_show_sticky_scroll: None,
                ui_editor_line_numbers: None,
                ui_editor_show_whitespace: None,
                language_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn merge_right_none_preserves_left() {
        let left = Settings {
            text_proto_log: Some(true),
            claude_default_placement: None,
            theme: None,
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            claude_permissions: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            ui_font_family: None,
            ui_font_size: None,
            ui_pane_show_tab_bar: None,
            ui_pane_show_breadcrumbs: None,
            ui_editor_show_scrollbar_markers: None,
            ui_editor_show_inline_blame: None,
            ui_editor_show_indent_guides: None,
            ui_editor_show_sticky_scroll: None,
            ui_editor_line_numbers: None,
            ui_editor_show_whitespace: None,
            language_servers: BTreeMap::new(),
        };
        let right = Settings::default();
        assert_eq!(
            left.merge(right),
            Settings {
                text_proto_log: Some(true),
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                claude_permissions: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                ui_font_family: None,
                ui_font_size: None,
                ui_pane_show_tab_bar: None,
                ui_pane_show_breadcrumbs: None,
                ui_editor_show_scrollbar_markers: None,
                ui_editor_show_inline_blame: None,
                ui_editor_show_indent_guides: None,
                ui_editor_show_sticky_scroll: None,
                ui_editor_line_numbers: None,
                ui_editor_show_whitespace: None,
                language_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn merge_both_none_is_none() {
        assert_eq!(
            Settings::default().merge(Settings::default()),
            Settings::default()
        );
    }

    #[test]
    fn from_config_extracts_claude_default_placement_pane() {
        let config = parse_ok(r#"on init { claude.default_placement = "pane"; }"#);
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: None,
                claude_default_placement: Some(ClaudePlacement::Pane),
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                claude_permissions: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                ui_font_family: None,
                ui_font_size: None,
                ui_pane_show_tab_bar: None,
                ui_pane_show_breadcrumbs: None,
                ui_editor_show_scrollbar_markers: None,
                ui_editor_show_inline_blame: None,
                ui_editor_show_indent_guides: None,
                ui_editor_show_sticky_scroll: None,
                ui_editor_line_numbers: None,
                ui_editor_show_whitespace: None,
                language_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn from_config_extracts_claude_default_placement_dock_left() {
        let config = parse_ok(r#"on init { claude.default_placement = "dock-left"; }"#);
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: None,
                claude_default_placement: Some(ClaudePlacement::DockLeft),
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                claude_permissions: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                ui_font_family: None,
                ui_font_size: None,
                ui_pane_show_tab_bar: None,
                ui_pane_show_breadcrumbs: None,
                ui_editor_show_scrollbar_markers: None,
                ui_editor_show_inline_blame: None,
                ui_editor_show_indent_guides: None,
                ui_editor_show_sticky_scroll: None,
                ui_editor_line_numbers: None,
                ui_editor_show_whitespace: None,
                language_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn from_config_extracts_claude_default_placement_dock_right() {
        let config = parse_ok(r#"on init { claude.default_placement = "dock-right"; }"#);
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: None,
                claude_default_placement: Some(ClaudePlacement::DockRight),
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                claude_permissions: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                ui_font_family: None,
                ui_font_size: None,
                ui_pane_show_tab_bar: None,
                ui_pane_show_breadcrumbs: None,
                ui_editor_show_scrollbar_markers: None,
                ui_editor_show_inline_blame: None,
                ui_editor_show_indent_guides: None,
                ui_editor_show_sticky_scroll: None,
                ui_editor_line_numbers: None,
                ui_editor_show_whitespace: None,
                language_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn from_config_ignores_unknown_placement_value() {
        let config = parse_ok(r#"on init { claude.default_placement = "elsewhere"; }"#);
        assert_eq!(Settings::from_config(&config), Settings::default());
    }

    #[test]
    fn from_config_ignores_wrong_type_placement_value() {
        let config = parse_ok("on init { claude.default_placement = true; }");
        assert_eq!(Settings::from_config(&config), Settings::default());
    }

    #[test]
    fn merge_preserves_claude_placement() {
        let left = Settings {
            text_proto_log: None,
            claude_default_placement: Some(ClaudePlacement::DockRight),
            theme: None,
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            claude_permissions: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            ui_font_family: None,
            ui_font_size: None,
            ui_pane_show_tab_bar: None,
            ui_pane_show_breadcrumbs: None,
            ui_editor_show_scrollbar_markers: None,
            ui_editor_show_inline_blame: None,
            ui_editor_show_indent_guides: None,
            ui_editor_show_sticky_scroll: None,
            ui_editor_line_numbers: None,
            ui_editor_show_whitespace: None,
            language_servers: BTreeMap::new(),
        };
        let right = Settings::default();
        assert_eq!(
            left.clone().merge(right),
            Settings {
                text_proto_log: None,
                claude_default_placement: Some(ClaudePlacement::DockRight),
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                claude_permissions: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                ui_font_family: None,
                ui_font_size: None,
                ui_pane_show_tab_bar: None,
                ui_pane_show_breadcrumbs: None,
                ui_editor_show_scrollbar_markers: None,
                ui_editor_show_inline_blame: None,
                ui_editor_show_indent_guides: None,
                ui_editor_show_sticky_scroll: None,
                ui_editor_line_numbers: None,
                ui_editor_show_whitespace: None,
                language_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn from_config_extracts_theme_ident() {
        let config = parse_ok("on init { theme = default_dark; }");
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: None,
                claude_default_placement: None,
                theme: Some("default_dark".into()),
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                claude_permissions: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                ui_font_family: None,
                ui_font_size: None,
                ui_pane_show_tab_bar: None,
                ui_pane_show_breadcrumbs: None,
                ui_editor_show_scrollbar_markers: None,
                ui_editor_show_inline_blame: None,
                ui_editor_show_indent_guides: None,
                ui_editor_show_sticky_scroll: None,
                ui_editor_line_numbers: None,
                ui_editor_show_whitespace: None,
                language_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn from_config_extracts_theme_string() {
        let config = parse_ok(r#"on init { theme = "default_dark"; }"#);
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: None,
                claude_default_placement: None,
                theme: Some("default_dark".into()),
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                claude_permissions: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                ui_font_family: None,
                ui_font_size: None,
                ui_pane_show_tab_bar: None,
                ui_pane_show_breadcrumbs: None,
                ui_editor_show_scrollbar_markers: None,
                ui_editor_show_inline_blame: None,
                ui_editor_show_indent_guides: None,
                ui_editor_show_sticky_scroll: None,
                ui_editor_line_numbers: None,
                ui_editor_show_whitespace: None,
                language_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn merge_right_overrides_theme() {
        let left = Settings {
            text_proto_log: None,
            claude_default_placement: None,
            theme: Some("a".into()),
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            claude_permissions: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            ui_font_family: None,
            ui_font_size: None,
            ui_pane_show_tab_bar: None,
            ui_pane_show_breadcrumbs: None,
            ui_editor_show_scrollbar_markers: None,
            ui_editor_show_inline_blame: None,
            ui_editor_show_indent_guides: None,
            ui_editor_show_sticky_scroll: None,
            ui_editor_line_numbers: None,
            ui_editor_show_whitespace: None,
            language_servers: BTreeMap::new(),
        };
        let right = Settings {
            text_proto_log: None,
            claude_default_placement: None,
            theme: Some("b".into()),
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            claude_permissions: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            ui_font_family: None,
            ui_font_size: None,
            ui_pane_show_tab_bar: None,
            ui_pane_show_breadcrumbs: None,
            ui_editor_show_scrollbar_markers: None,
            ui_editor_show_inline_blame: None,
            ui_editor_show_indent_guides: None,
            ui_editor_show_sticky_scroll: None,
            ui_editor_line_numbers: None,
            ui_editor_show_whitespace: None,
            language_servers: BTreeMap::new(),
        };
        assert_eq!(left.merge(right).theme, Some("b".into()));
    }

    #[test]
    fn merge_right_overrides_claude_placement() {
        let left = Settings {
            text_proto_log: None,
            claude_default_placement: Some(ClaudePlacement::Pane),
            theme: None,
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            claude_permissions: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            ui_font_family: None,
            ui_font_size: None,
            ui_pane_show_tab_bar: None,
            ui_pane_show_breadcrumbs: None,
            ui_editor_show_scrollbar_markers: None,
            ui_editor_show_inline_blame: None,
            ui_editor_show_indent_guides: None,
            ui_editor_show_sticky_scroll: None,
            ui_editor_line_numbers: None,
            ui_editor_show_whitespace: None,
            language_servers: BTreeMap::new(),
        };
        let right = Settings {
            text_proto_log: None,
            claude_default_placement: Some(ClaudePlacement::DockLeft),
            theme: None,
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            claude_permissions: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            ui_font_family: None,
            ui_font_size: None,
            ui_pane_show_tab_bar: None,
            ui_pane_show_breadcrumbs: None,
            ui_editor_show_scrollbar_markers: None,
            ui_editor_show_inline_blame: None,
            ui_editor_show_indent_guides: None,
            ui_editor_show_sticky_scroll: None,
            ui_editor_line_numbers: None,
            ui_editor_show_whitespace: None,
            language_servers: BTreeMap::new(),
        };
        assert_eq!(
            left.merge(right),
            Settings {
                text_proto_log: None,
                claude_default_placement: Some(ClaudePlacement::DockLeft),
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                claude_permissions: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                ui_font_family: None,
                ui_font_size: None,
                ui_pane_show_tab_bar: None,
                ui_pane_show_breadcrumbs: None,
                ui_editor_show_scrollbar_markers: None,
                ui_editor_show_inline_blame: None,
                ui_editor_show_indent_guides: None,
                ui_editor_show_sticky_scroll: None,
                ui_editor_line_numbers: None,
                ui_editor_show_whitespace: None,
                language_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn from_config_extracts_mouse_capture_auto() {
        let config = parse_ok(r#"on init { mouse.capture = "auto"; }"#);
        assert_eq!(
            Settings::from_config(&config).mouse_capture,
            Some(MouseCapturePolicy::Auto),
        );
    }

    #[test]
    fn from_config_extracts_mouse_capture_always() {
        let config = parse_ok(r#"on init { mouse.capture = "always"; }"#);
        assert_eq!(
            Settings::from_config(&config).mouse_capture,
            Some(MouseCapturePolicy::Always),
        );
    }

    #[test]
    fn from_config_extracts_mouse_capture_never_ident() {
        let config = parse_ok("on init { mouse.capture = never; }");
        assert_eq!(
            Settings::from_config(&config).mouse_capture,
            Some(MouseCapturePolicy::Never),
        );
    }

    #[test]
    fn from_config_ignores_unknown_mouse_capture_value() {
        let config = parse_ok(r#"on init { mouse.capture = "elsewhere"; }"#);
        assert_eq!(Settings::from_config(&config).mouse_capture, None);
    }

    #[test]
    fn merge_right_overrides_mouse_capture() {
        let left = Settings {
            mouse_capture: Some(MouseCapturePolicy::Auto),
            ..Settings::default()
        };
        let right = Settings {
            mouse_capture: Some(MouseCapturePolicy::Never),
            ..Settings::default()
        };
        assert_eq!(
            left.merge(right).mouse_capture,
            Some(MouseCapturePolicy::Never)
        );
    }

    #[test]
    fn from_config_extracts_mode_badge_string() {
        let config = parse_ok(r#"on init { ui.mode_badge.foo = "FOO"; }"#);
        let badges = Settings::from_config(&config).mode_badges;
        assert_eq!(
            badges,
            BTreeMap::from([("foo".to_string(), "FOO".to_string())])
        );
    }

    #[test]
    fn from_config_extracts_mode_badge_ident() {
        let config = parse_ok("on init { ui.mode_badge.bar = BAR; }");
        let badges = Settings::from_config(&config).mode_badges;
        assert_eq!(
            badges,
            BTreeMap::from([("bar".to_string(), "BAR".to_string())])
        );
    }

    #[test]
    fn from_config_extracts_multiple_mode_badges() {
        let config = parse_ok(
            r#"on init {
                ui.mode_badge.foo = "FOO";
                ui.mode_badge.bar = "BAR";
            }"#,
        );
        let badges = Settings::from_config(&config).mode_badges;
        assert_eq!(
            badges,
            BTreeMap::from([
                ("foo".to_string(), "FOO".to_string()),
                ("bar".to_string(), "BAR".to_string()),
            ])
        );
    }

    #[test]
    fn from_config_ignores_wrong_type_mode_badge() {
        let config = parse_ok("on init { ui.mode_badge.foo = true; }");
        assert!(Settings::from_config(&config).mode_badges.is_empty());
    }

    #[test]
    fn merge_extends_mode_badges_with_right_winning() {
        let left = Settings {
            mode_badges: BTreeMap::from([
                ("foo".to_string(), "FOO".to_string()),
                ("shared".to_string(), "L".to_string()),
            ]),
            ..Settings::default()
        };
        let right = Settings {
            mode_badges: BTreeMap::from([
                ("bar".to_string(), "BAR".to_string()),
                ("shared".to_string(), "R".to_string()),
            ]),
            ..Settings::default()
        };
        assert_eq!(
            left.merge(right).mode_badges,
            BTreeMap::from([
                ("foo".to_string(), "FOO".to_string()),
                ("bar".to_string(), "BAR".to_string()),
                ("shared".to_string(), "R".to_string()),
            ])
        );
    }

    #[test]
    fn from_config_extracts_claude_permissions() {
        let config = parse_ok(
            r#"on init {
                claude.permissions.Bash.always_allow = ["^cargo (build|test)"];
                claude.permissions.Bash.always_deny = ["^sudo "];
                claude.permissions.Read.always_confirm = ["secrets/.*"];
            }"#,
        );
        let settings = Settings::from_config(&config);
        let bash = settings.claude_permissions.get("Bash").expect("Bash entry");
        assert_eq!(bash.always_allow, vec!["^cargo (build|test)".to_string()]);
        assert_eq!(bash.always_deny, vec!["^sudo ".to_string()]);
        assert!(bash.always_confirm.is_empty());
        let read = settings.claude_permissions.get("Read").expect("Read entry");
        assert_eq!(read.always_confirm, vec!["secrets/.*".to_string()]);
    }

    #[test]
    fn from_config_ignores_non_string_permission_items() {
        let config = parse_ok(
            r#"on init {
                claude.permissions.Bash.always_allow = ["^cargo", 42, true];
            }"#,
        );
        let settings = Settings::from_config(&config);
        let bash = settings.claude_permissions.get("Bash").expect("Bash entry");
        assert_eq!(bash.always_allow, vec!["^cargo".to_string()]);
    }

    #[test]
    fn from_config_ignores_non_array_permission_value() {
        let config = parse_ok(
            r#"on init {
                claude.permissions.Bash.always_allow = "not-an-array";
            }"#,
        );
        assert!(Settings::from_config(&config).claude_permissions.is_empty());
    }

    #[test]
    fn from_config_ignores_unknown_permission_behavior() {
        let config = parse_ok(
            r#"on init {
                claude.permissions.Bash.never_allow = ["^cargo"];
            }"#,
        );
        let settings = Settings::from_config(&config);
        let bash = settings.claude_permissions.get("Bash").expect("Bash entry");
        assert!(bash.always_allow.is_empty());
        assert!(bash.always_confirm.is_empty());
        assert!(bash.always_deny.is_empty());
    }

    #[test]
    fn merge_claude_permissions_right_wins_per_tool() {
        let left = Settings {
            claude_permissions: BTreeMap::from([(
                "Bash".to_string(),
                ToolPermissions {
                    always_allow: vec!["^left".to_string()],
                    always_confirm: vec![],
                    always_deny: vec![],
                },
            )]),
            ..Settings::default()
        };
        let right = Settings {
            claude_permissions: BTreeMap::from([(
                "Bash".to_string(),
                ToolPermissions {
                    always_allow: vec!["^right".to_string()],
                    always_confirm: vec![],
                    always_deny: vec![],
                },
            )]),
            ..Settings::default()
        };
        let merged = left.merge(right);
        assert_eq!(
            merged.claude_permissions.get("Bash").unwrap().always_allow,
            vec!["^right".to_string()]
        );
    }

    #[test]
    fn merge_claude_permissions_layers_disjoint_tools() {
        let left = Settings {
            claude_permissions: BTreeMap::from([(
                "Bash".to_string(),
                ToolPermissions {
                    always_allow: vec!["^cargo".to_string()],
                    ..Default::default()
                },
            )]),
            ..Settings::default()
        };
        let right = Settings {
            claude_permissions: BTreeMap::from([(
                "Read".to_string(),
                ToolPermissions {
                    always_deny: vec!["secrets/".to_string()],
                    ..Default::default()
                },
            )]),
            ..Settings::default()
        };
        let merged = left.merge(right);
        assert!(merged.claude_permissions.contains_key("Bash"));
        assert!(merged.claude_permissions.contains_key("Read"));
    }

    #[test]
    fn from_config_extracts_editor_font_family() {
        let config = parse_ok(r#"on init { editor.font.family = "Menlo"; }"#);
        assert_eq!(
            Settings::from_config(&config).editor_font_family,
            Some("Menlo".to_string()),
        );
    }

    #[test]
    fn from_config_extracts_editor_font_size() {
        let config = parse_ok("on init { editor.font.size = 13; }");
        assert_eq!(Settings::from_config(&config).editor_font_size, Some(13.0));
    }

    #[test]
    fn from_config_extracts_ui_font_family() {
        let config = parse_ok(r#"on init { ui.font.family = "SF Pro"; }"#);
        assert_eq!(
            Settings::from_config(&config).ui_font_family,
            Some("SF Pro".to_string()),
        );
    }

    #[test]
    fn from_config_extracts_ui_font_size() {
        let config = parse_ok("on init { ui.font.size = 15; }");
        assert_eq!(Settings::from_config(&config).ui_font_size, Some(15.0));
    }

    #[test]
    fn from_config_ignores_wrong_type_font_family() {
        let config = parse_ok("on init { editor.font.family = 12; }");
        assert!(Settings::from_config(&config).editor_font_family.is_none());
    }

    #[test]
    fn from_config_ignores_wrong_type_font_size() {
        let config = parse_ok(r#"on init { editor.font.size = "big"; }"#);
        assert!(Settings::from_config(&config).editor_font_size.is_none());
    }

    #[test]
    fn merge_right_overrides_editor_font_family() {
        let left = Settings {
            editor_font_family: Some("Menlo".into()),
            ..Settings::default()
        };
        let right = Settings {
            editor_font_family: Some("Cascadia".into()),
            ..Settings::default()
        };
        assert_eq!(
            left.merge(right).editor_font_family,
            Some("Cascadia".to_string()),
        );
    }

    #[test]
    fn merge_right_none_preserves_font_fields() {
        let left = Settings {
            editor_font_family: Some("Menlo".into()),
            editor_font_size: Some(13.0),
            ui_font_family: Some("SF Pro".into()),
            ui_font_size: Some(14.0),
            ..Settings::default()
        };
        let merged = left.merge(Settings::default());
        assert_eq!(merged.editor_font_family, Some("Menlo".to_string()));
        assert_eq!(merged.editor_font_size, Some(13.0));
        assert_eq!(merged.ui_font_family, Some("SF Pro".to_string()));
        assert_eq!(merged.ui_font_size, Some(14.0));
    }

    #[test]
    fn from_config_extracts_lsp_command() {
        let config = parse_ok(r#"on init { lsp.rust.command = "rust-analyzer"; }"#);
        let settings = Settings::from_config(&config);
        let rust = settings.language_servers.get("rust").expect("rust entry");
        assert_eq!(rust.command, "rust-analyzer");
        assert!(rust.args.is_empty());
        assert!(rust.env.is_empty());
    }

    #[test]
    fn from_config_extracts_lsp_args() {
        let config = parse_ok(
            r#"on init {
                lsp.rust.args = ["--stdio", "--log", "info"];
            }"#,
        );
        let settings = Settings::from_config(&config);
        let rust = settings.language_servers.get("rust").expect("rust entry");
        assert_eq!(
            rust.args,
            vec![
                "--stdio".to_string(),
                "--log".to_string(),
                "info".to_string(),
            ],
        );
    }

    #[test]
    fn from_config_extracts_lsp_env_entries() {
        let config = parse_ok(
            r#"on init {
                lsp.rust.env.RUST_LOG = "debug";
                lsp.rust.env.RA_LOG = "trace";
            }"#,
        );
        let settings = Settings::from_config(&config);
        let rust = settings.language_servers.get("rust").expect("rust entry");
        assert_eq!(
            rust.env,
            BTreeMap::from([
                ("RUST_LOG".to_string(), "debug".to_string()),
                ("RA_LOG".to_string(), "trace".to_string()),
            ]),
        );
    }

    #[test]
    fn from_config_combines_lsp_subfields_for_same_language() {
        let config = parse_ok(
            r#"on init {
                lsp.rust.command = "rust-analyzer";
                lsp.rust.args = ["--stdio"];
                lsp.rust.env.RUST_LOG = "info";
            }"#,
        );
        let settings = Settings::from_config(&config);
        let rust = settings.language_servers.get("rust").expect("rust entry");
        assert_eq!(
            rust,
            &LanguageServerCommand {
                command: "rust-analyzer".to_string(),
                args: vec!["--stdio".to_string()],
                env: BTreeMap::from([("RUST_LOG".to_string(), "info".to_string())]),
            },
        );
    }

    #[test]
    fn from_config_keeps_lsp_entries_per_language() {
        let config = parse_ok(
            r#"on init {
                lsp.rust.command = "rust-analyzer";
                lsp.typescript.command = "typescript-language-server";
                lsp.typescript.args = ["--stdio"];
            }"#,
        );
        let settings = Settings::from_config(&config);
        assert_eq!(settings.language_servers.len(), 2);
        assert_eq!(
            settings
                .language_servers
                .get("rust")
                .map(|e| e.command.as_str()),
            Some("rust-analyzer"),
        );
        let ts = settings
            .language_servers
            .get("typescript")
            .expect("typescript entry");
        assert_eq!(ts.command, "typescript-language-server");
        assert_eq!(ts.args, vec!["--stdio".to_string()]);
    }

    #[test]
    fn from_config_ignores_lsp_command_with_wrong_value_type() {
        let config = parse_ok("on init { lsp.rust.command = 42; }");
        assert!(Settings::from_config(&config).language_servers.is_empty());
    }

    #[test]
    fn from_config_ignores_lsp_args_with_non_array_value() {
        let config = parse_ok(r#"on init { lsp.rust.args = "not-an-array"; }"#);
        assert!(Settings::from_config(&config).language_servers.is_empty());
    }

    #[test]
    fn from_config_ignores_non_string_lsp_args_items() {
        let config = parse_ok(r#"on init { lsp.rust.args = ["--stdio", 42, true]; }"#);
        let settings = Settings::from_config(&config);
        let rust = settings.language_servers.get("rust").expect("rust entry");
        assert_eq!(rust.args, vec!["--stdio".to_string()]);
    }

    #[test]
    fn merge_language_servers_replaces_per_key() {
        let left = Settings {
            language_servers: BTreeMap::from([
                (
                    "rust".to_string(),
                    LanguageServerCommand {
                        command: "rust-analyzer".to_string(),
                        args: vec!["--left".to_string()],
                        env: BTreeMap::new(),
                    },
                ),
                (
                    "python".to_string(),
                    LanguageServerCommand {
                        command: "pyright".to_string(),
                        args: vec![],
                        env: BTreeMap::new(),
                    },
                ),
            ]),
            ..Settings::default()
        };
        let right = Settings {
            language_servers: BTreeMap::from([(
                "rust".to_string(),
                LanguageServerCommand {
                    command: "rust-analyzer-override".to_string(),
                    args: vec![],
                    env: BTreeMap::from([("RUST_LOG".to_string(), "warn".to_string())]),
                },
            )]),
            ..Settings::default()
        };
        let merged = left.merge(right);
        assert_eq!(
            merged.language_servers.get("rust"),
            Some(&LanguageServerCommand {
                command: "rust-analyzer-override".to_string(),
                args: vec![],
                env: BTreeMap::from([("RUST_LOG".to_string(), "warn".to_string())]),
            }),
        );
        assert_eq!(
            merged.language_servers.get("python"),
            Some(&LanguageServerCommand {
                command: "pyright".to_string(),
                args: vec![],
                env: BTreeMap::new(),
            }),
        );
    }
}
