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
    /// Re-apply the user config automatically when it is saved in the
    /// editor. `None` defaults to enabled, so the edit-and-save loop
    /// works without opting in.
    pub auto_reload_config: Option<bool>,
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
    /// Monospace font family for the editor pane. Set via
    /// `editor.font.family = "Menlo";`.
    pub editor_font_family: Option<String>,
    /// Editor pane font size in logical pixels. Set via
    /// `editor.font.size = 14;`.
    pub editor_font_size: Option<f32>,
    /// Monospace font family for the terminal pane. `None` falls back to
    /// the editor font family. Set via `terminal.font.family = "Fira Code";`.
    pub terminal_font_family: Option<String>,
    /// Terminal pane font size in logical pixels. `None` falls back to the
    /// editor font size. Set via `terminal.font.size = 14;`.
    pub terminal_font_size: Option<f32>,
    /// Render programming ligatures in the terminal pane. `None` defaults to
    /// enabled. Set via `terminal.font.ligatures = false;` to disable.
    pub terminal_font_ligatures: Option<bool>,
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
    /// Per-pane-item-kind input mode to land in when an item of that
    /// kind becomes active, keyed by the kind's lowercase name (e.g.
    /// `editor`, `review`, `rebase`). Set via `ui.item_mode.<kind> =
    /// "<mode>";`. Consumers fall back to a built-in default for kinds
    /// absent from the map. Right-hand wins on merge per key.
    pub item_modes: BTreeMap<String, String>,
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
        let mut language_servers = self.language_servers;
        language_servers.extend(other.language_servers);
        let mut item_modes = self.item_modes;
        item_modes.extend(other.item_modes);
        Settings {
            text_proto_log: other.text_proto_log.or(self.text_proto_log),
            auto_reload_config: other.auto_reload_config.or(self.auto_reload_config),
            claude_default_placement: other
                .claude_default_placement
                .or(self.claude_default_placement),
            theme: other.theme.or(self.theme),
            mouse_capture: other.mouse_capture.or(self.mouse_capture),
            mode_badges,
            editor_font_family: other.editor_font_family.or(self.editor_font_family),
            editor_font_size: other.editor_font_size.or(self.editor_font_size),
            terminal_font_family: other.terminal_font_family.or(self.terminal_font_family),
            terminal_font_size: other.terminal_font_size.or(self.terminal_font_size),
            terminal_font_ligatures: other
                .terminal_font_ligatures
                .or(self.terminal_font_ligatures),
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
            item_modes,
        }
    }

    fn apply(&mut self, setting: &Setting) {
        let path: Vec<&str> = setting.path.iter().map(|p| p.node.as_str()).collect();
        for def in setting_catalog() {
            if let Some(wildcards) = match_setting_key(def.key, &path) {
                (def.apply_value)(self, &wildcards, &setting.value.node);
                return;
            }
        }
        // Unknown keys are silently ignored so a config referencing a future
        // setting on an older binary still parses.
    }

    /// Apply a `<path> = <raw>` assignment at runtime, parsing the
    /// stringly value the same way the palette's `Set` action would.
    /// The `path` argument is the dotted setting key (e.g.
    /// `ui.pane.show_tab_bar`); `raw` is the unparsed value as the
    /// user typed it.
    pub fn apply_runtime(&mut self, path: &str, raw: &str) -> Result<(), SettingsApplyError> {
        let tokens: Vec<&str> = path.split('.').collect();
        for def in setting_catalog() {
            let Some(wildcards) = match_setting_key(def.key, &tokens) else {
                continue;
            };
            let Some(apply_raw) = def.apply_raw else {
                break;
            };
            return if apply_raw(self, &wildcards, raw) {
                Ok(())
            } else {
                InvalidValueSnafu {
                    key: path.to_string(),
                    expected: def.expected(),
                    got: raw.to_string(),
                }
                .fail()
            };
        }
        UnknownKeySnafu {
            key: path.to_string(),
        }
        .fail()
    }
}

/// Value shape a setting key accepts. Drives validation and is exposed to
/// editor tooling (completion, hover, diagnostics) so the accepted shape is
/// described in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Bool,
    Number,
    Str,
    /// One of a fixed set of identifiers; the variants are [`SettingDef::allowed`].
    Enum,
    StringArray,
}

/// Applies a parsed config value to [`Settings`]. The slice is the concrete
/// segments a [`SettingDef`] key's `*` wildcards matched, in order.
type ApplyValueFn = fn(&mut Settings, &[&str], &Value);

/// Applies a raw runtime string to [`Settings`], returning `false` when the
/// string does not parse. The slice carries the wildcard segments as in
/// [`ApplyValueFn`].
type ApplyRawFn = fn(&mut Settings, &[&str], &str) -> bool;

/// One recognized stcfg setting key. The catalog of these is the single
/// source of truth behind both apply paths and the editor tooling, so the
/// recognized keys, their value shapes, allowed enum values, and docs cannot
/// drift from what the parser accepts.
///
/// `key` is the dotted path with `*` marking a wildcard segment, e.g.
/// `ui.mode_badge.*` or `lsp.*.env.*`; the concrete segments a wildcard
/// matches are passed to the apply functions in order.
pub struct SettingDef {
    pub key: &'static str,
    pub kind: ValueKind,
    /// Allowed identifiers for [`ValueKind::Enum`] keys, else empty.
    pub allowed: &'static [&'static str],
    pub doc: &'static str,
    /// Apply a parsed config value (the `on init` path). Unrecognized value
    /// shapes are ignored, matching the parser's forward-compatible stance.
    apply_value: ApplyValueFn,
    /// Apply a raw string at runtime (the palette `Set` path). Returns `false`
    /// when `raw` does not parse. `None` for keys that are not runtime-settable.
    apply_raw: Option<ApplyRawFn>,
}

impl SettingDef {
    /// Human-readable description of the accepted value, for the
    /// `InvalidValue` error. Enum keys list their variants.
    fn expected(&self) -> String {
        match self.kind {
            ValueKind::Bool => "bool".to_string(),
            ValueKind::Number => "number".to_string(),
            ValueKind::Str => "string".to_string(),
            ValueKind::StringArray => "array of strings".to_string(),
            ValueKind::Enum => self.allowed.join("|"),
        }
    }
}

/// The recognized stcfg setting keys. Both [`Settings::apply`] and
/// [`Settings::apply_runtime`] dispatch through this table, and editor tooling
/// reads it for completion, hover, and unknown-key diagnostics.
pub fn setting_catalog() -> &'static [SettingDef] {
    SETTING_CATALOG
}

/// Match a catalog key (dotted, with `*` wildcards) against a setting path,
/// returning the concrete segments captured by the wildcards in order, or
/// `None` when the key does not apply. The lengths must match exactly.
fn match_setting_key<'a>(key: &str, path: &[&'a str]) -> Option<Vec<&'a str>> {
    let segments: Vec<&str> = key.split('.').collect();
    if segments.len() != path.len() {
        return None;
    }
    let mut wildcards = Vec::new();
    for (seg, actual) in segments.iter().zip(path.iter()) {
        if *seg == "*" {
            wildcards.push(*actual);
        } else if seg != actual {
            return None;
        }
    }
    Some(wildcards)
}

static SETTING_CATALOG: &[SettingDef] = &[
    SettingDef {
        key: "text_proto_log",
        kind: ValueKind::Bool,
        allowed: &[],
        doc: "Enable the Claude Code / LSP text-protocol transcript log.",
        apply_value: |s, _w, v| {
            if let Value::Bool(b) = v {
                s.text_proto_log = Some(*b);
            }
        },
        apply_raw: None,
    },
    SettingDef {
        key: "auto_reload_config",
        kind: ValueKind::Bool,
        allowed: &[],
        doc: "Re-apply the user config automatically when it is saved in the editor.",
        apply_value: |s, _w, v| {
            if let Value::Bool(b) = v {
                s.auto_reload_config = Some(*b);
            }
        },
        apply_raw: Some(|s, _w, raw| match parse_bool(raw) {
            Some(b) => {
                s.auto_reload_config = Some(b);
                true
            },
            None => false,
        }),
    },
    SettingDef {
        key: "claude.default_placement",
        kind: ValueKind::Enum,
        allowed: &["pane", "dock-left", "dock-right"],
        doc: "Default placement of a newly-opened Claude chat (OpenClaude).",
        apply_value: |s, _w, v| {
            let raw = match v {
                Value::String(x) | Value::Ident(x) => Some(x.as_str()),
                _ => None,
            };
            let placement = match raw {
                Some("pane") => Some(ClaudePlacement::Pane),
                Some("dock-left") => Some(ClaudePlacement::DockLeft),
                Some("dock-right") => Some(ClaudePlacement::DockRight),
                _ => None,
            };
            if let Some(p) = placement {
                s.claude_default_placement = Some(p);
            }
        },
        apply_raw: None,
    },
    SettingDef {
        key: "theme",
        kind: ValueKind::Str,
        allowed: &[],
        doc: "Name of the active theme block.",
        apply_value: |s, _w, v| {
            if let Value::Ident(x) | Value::String(x) = v {
                s.theme = Some(x.clone());
            }
        },
        apply_raw: None,
    },
    SettingDef {
        key: "ui.mode_badge.*",
        kind: ValueKind::Str,
        allowed: &[],
        doc: "Status-line badge label for a mode, keyed by mode name.",
        apply_value: |s, w, v| {
            if let Value::String(badge) | Value::Ident(badge) = v {
                s.mode_badges.insert(w[0].to_string(), badge.clone());
            }
        },
        apply_raw: None,
    },
    SettingDef {
        key: "ui.item_mode.*",
        kind: ValueKind::Str,
        allowed: &[],
        doc: "Input mode to enter when a pane item of the given kind becomes active.",
        apply_value: |s, w, v| {
            if let Value::String(mode) | Value::Ident(mode) = v {
                s.item_modes.insert(w[0].to_string(), mode.clone());
            }
        },
        apply_raw: None,
    },
    SettingDef {
        key: "mouse.capture",
        kind: ValueKind::Enum,
        allowed: &["auto", "always", "never"],
        doc: "Mouse-capture policy at terminal startup.",
        apply_value: |s, _w, v| {
            let raw = match v {
                Value::String(x) | Value::Ident(x) => Some(x.as_str()),
                _ => None,
            };
            let policy = match raw {
                Some("auto") => Some(MouseCapturePolicy::Auto),
                Some("always") => Some(MouseCapturePolicy::Always),
                Some("never") => Some(MouseCapturePolicy::Never),
                _ => None,
            };
            if let Some(p) = policy {
                s.mouse_capture = Some(p);
            }
        },
        apply_raw: None,
    },
    SettingDef {
        key: "editor.font.family",
        kind: ValueKind::Str,
        allowed: &[],
        doc: "Monospace font family for the editor pane.",
        apply_value: |s, _w, v| {
            if let Value::String(x) = v {
                s.editor_font_family = Some(x.clone());
            }
        },
        apply_raw: Some(|s, _w, raw| {
            s.editor_font_family = Some(raw.to_string());
            true
        }),
    },
    SettingDef {
        key: "editor.font.size",
        kind: ValueKind::Number,
        allowed: &[],
        doc: "Editor pane font size, in logical pixels.",
        apply_value: |s, _w, v| {
            if let Value::Number(n) = v {
                s.editor_font_size = Some(*n as f32);
            }
        },
        apply_raw: Some(|s, _w, raw| match raw.parse::<f32>() {
            Ok(n) => {
                s.editor_font_size = Some(n);
                true
            },
            Err(_) => false,
        }),
    },
    SettingDef {
        key: "terminal.font.family",
        kind: ValueKind::Str,
        allowed: &[],
        doc: "Monospace font family for the terminal pane. Falls back to the editor font family.",
        apply_value: |s, _w, v| {
            if let Value::String(x) = v {
                s.terminal_font_family = Some(x.clone());
            }
        },
        apply_raw: Some(|s, _w, raw| {
            s.terminal_font_family = Some(raw.to_string());
            true
        }),
    },
    SettingDef {
        key: "terminal.font.size",
        kind: ValueKind::Number,
        allowed: &[],
        doc: "Terminal pane font size, in logical pixels. Falls back to the editor font size.",
        apply_value: |s, _w, v| {
            if let Value::Number(n) = v {
                s.terminal_font_size = Some(*n as f32);
            }
        },
        apply_raw: Some(|s, _w, raw| match raw.parse::<f32>() {
            Ok(n) => {
                s.terminal_font_size = Some(n);
                true
            },
            Err(_) => false,
        }),
    },
    SettingDef {
        key: "terminal.font.ligatures",
        kind: ValueKind::Bool,
        allowed: &[],
        doc: "Render programming ligatures in the terminal pane.",
        apply_value: |s, _w, v| {
            if let Value::Bool(b) = v {
                s.terminal_font_ligatures = Some(*b);
            }
        },
        apply_raw: Some(|s, _w, raw| match parse_bool(raw) {
            Some(b) => {
                s.terminal_font_ligatures = Some(b);
                true
            },
            None => false,
        }),
    },
    SettingDef {
        key: "ui.font.family",
        kind: ValueKind::Str,
        allowed: &[],
        doc: "Proportional font family for chrome (status bar, tab bar, modals, docks).",
        apply_value: |s, _w, v| {
            if let Value::String(x) = v {
                s.ui_font_family = Some(x.clone());
            }
        },
        apply_raw: Some(|s, _w, raw| {
            s.ui_font_family = Some(raw.to_string());
            true
        }),
    },
    SettingDef {
        key: "ui.font.size",
        kind: ValueKind::Number,
        allowed: &[],
        doc: "Chrome font size, in logical pixels.",
        apply_value: |s, _w, v| {
            if let Value::Number(n) = v {
                s.ui_font_size = Some(*n as f32);
            }
        },
        apply_raw: Some(|s, _w, raw| match raw.parse::<f32>() {
            Ok(n) => {
                s.ui_font_size = Some(n);
                true
            },
            Err(_) => false,
        }),
    },
    SettingDef {
        key: "ui.pane.show_tab_bar",
        kind: ValueKind::Bool,
        allowed: &[],
        doc: "Show the per-pane tab bar above editor content.",
        apply_value: |s, _w, v| {
            if let Value::Bool(b) = v {
                s.ui_pane_show_tab_bar = Some(*b);
            }
        },
        apply_raw: Some(|s, _w, raw| match parse_bool(raw) {
            Some(b) => {
                s.ui_pane_show_tab_bar = Some(b);
                true
            },
            None => false,
        }),
    },
    SettingDef {
        key: "ui.pane.show_breadcrumbs",
        kind: ValueKind::Bool,
        allowed: &[],
        doc: "Show the per-pane breadcrumbs bar above editor content.",
        apply_value: |s, _w, v| {
            if let Value::Bool(b) = v {
                s.ui_pane_show_breadcrumbs = Some(*b);
            }
        },
        apply_raw: Some(|s, _w, raw| match parse_bool(raw) {
            Some(b) => {
                s.ui_pane_show_breadcrumbs = Some(b);
                true
            },
            None => false,
        }),
    },
    SettingDef {
        key: "ui.editor.show_scrollbar_markers",
        kind: ValueKind::Bool,
        allowed: &[],
        doc: "Paint diagnostic, git-hunk, and search markers on the editor scrollbar.",
        apply_value: |s, _w, v| {
            if let Value::Bool(b) = v {
                s.ui_editor_show_scrollbar_markers = Some(*b);
            }
        },
        apply_raw: Some(|s, _w, raw| match parse_bool(raw) {
            Some(b) => {
                s.ui_editor_show_scrollbar_markers = Some(b);
                true
            },
            None => false,
        }),
    },
    SettingDef {
        key: "ui.editor.show_inline_blame",
        kind: ValueKind::Bool,
        allowed: &[],
        doc: "Show inline git blame (author and relative age) at the end of each editable line.",
        apply_value: |s, _w, v| {
            if let Value::Bool(b) = v {
                s.ui_editor_show_inline_blame = Some(*b);
            }
        },
        apply_raw: Some(|s, _w, raw| match parse_bool(raw) {
            Some(b) => {
                s.ui_editor_show_inline_blame = Some(b);
                true
            },
            None => false,
        }),
    },
    SettingDef {
        key: "ui.editor.show_indent_guides",
        kind: ValueKind::Bool,
        allowed: &[],
        doc: "Show indent guides (vertical lines at tab-stop columns) in the editor.",
        apply_value: |s, _w, v| {
            if let Value::Bool(b) = v {
                s.ui_editor_show_indent_guides = Some(*b);
            }
        },
        apply_raw: Some(|s, _w, raw| match parse_bool(raw) {
            Some(b) => {
                s.ui_editor_show_indent_guides = Some(b);
                true
            },
            None => false,
        }),
    },
    SettingDef {
        key: "ui.editor.show_sticky_scroll",
        kind: ValueKind::Bool,
        allowed: &[],
        doc: "Show sticky scroll headers (pin the enclosing container's signature at the top).",
        apply_value: |s, _w, v| {
            if let Value::Bool(b) = v {
                s.ui_editor_show_sticky_scroll = Some(*b);
            }
        },
        apply_raw: Some(|s, _w, raw| match parse_bool(raw) {
            Some(b) => {
                s.ui_editor_show_sticky_scroll = Some(b);
                true
            },
            None => false,
        }),
    },
    SettingDef {
        key: "ui.editor.line_numbers",
        kind: ValueKind::Enum,
        allowed: &["absolute", "relative", "hybrid"],
        doc: "Gutter line-number display mode.",
        apply_value: |s, _w, v| {
            let mode = match v {
                Value::String(x) | Value::Ident(x) => parse_line_number_mode(x),
                _ => None,
            };
            if let Some(mode) = mode {
                s.ui_editor_line_numbers = Some(mode);
            }
        },
        apply_raw: Some(|s, _w, raw| match parse_line_number_mode(raw) {
            Some(mode) => {
                s.ui_editor_line_numbers = Some(mode);
                true
            },
            None => false,
        }),
    },
    SettingDef {
        key: "ui.editor.show_whitespace",
        kind: ValueKind::Enum,
        allowed: &["none", "boundary", "selection", "all"],
        doc: "Visible-whitespace rendering mode.",
        apply_value: |s, _w, v| {
            let mode = match v {
                Value::String(x) | Value::Ident(x) => parse_show_whitespace(x),
                _ => None,
            };
            if let Some(mode) = mode {
                s.ui_editor_show_whitespace = Some(mode);
            }
        },
        apply_raw: Some(|s, _w, raw| match parse_show_whitespace(raw) {
            Some(mode) => {
                s.ui_editor_show_whitespace = Some(mode);
                true
            },
            None => false,
        }),
    },
    SettingDef {
        key: "lsp.*.command",
        kind: ValueKind::Str,
        allowed: &[],
        doc: "LSP server command for a language, keyed by language name.",
        apply_value: |s, w, v| {
            if let Value::String(x) = v {
                s.language_servers
                    .entry(w[0].to_string())
                    .or_default()
                    .command = x.clone();
            }
        },
        apply_raw: None,
    },
    SettingDef {
        key: "lsp.*.args",
        kind: ValueKind::StringArray,
        allowed: &[],
        doc: "LSP server arguments for a language, keyed by language name.",
        apply_value: |s, w, v| {
            let Value::Array(items) = v else {
                return;
            };
            let args: Vec<String> = items
                .iter()
                .filter_map(|item| match &item.node {
                    Value::String(x) => Some(x.clone()),
                    _ => None,
                })
                .collect();
            s.language_servers.entry(w[0].to_string()).or_default().args = args;
        },
        apply_raw: None,
    },
    SettingDef {
        key: "lsp.*.env.*",
        kind: ValueKind::Str,
        allowed: &[],
        doc: "Environment variable exported into a language's LSP child process.",
        apply_value: |s, w, v| {
            if let Value::String(x) = v {
                s.language_servers
                    .entry(w[0].to_string())
                    .or_default()
                    .env
                    .insert(w[1].to_string(), x.clone());
            }
        },
        apply_raw: None,
    },
];

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
        expected: String,
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
    fn from_config_extracts_auto_reload_config() {
        let config = parse_ok("on init { auto_reload_config = false; }");
        let settings = Settings::from_config(&config);
        assert_eq!(settings.auto_reload_config, Some(false));
    }

    #[test]
    fn from_config_extracts_text_proto_log() {
        let config = parse_ok("on init { text_proto_log = true; }");
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: Some(true),
                auto_reload_config: None,
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                terminal_font_family: None,
                terminal_font_size: None,
                terminal_font_ligatures: None,
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
                item_modes: BTreeMap::new(),
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
                auto_reload_config: None,
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                terminal_font_family: None,
                terminal_font_size: None,
                terminal_font_ligatures: None,
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
                item_modes: BTreeMap::new(),
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
                auto_reload_config: None,
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                terminal_font_family: None,
                terminal_font_size: None,
                terminal_font_ligatures: None,
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
                item_modes: BTreeMap::new(),
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
            auto_reload_config: None,
            claude_default_placement: None,
            theme: None,
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            terminal_font_family: None,
            terminal_font_size: None,
            terminal_font_ligatures: None,
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
            item_modes: BTreeMap::new(),
        };
        let right = Settings {
            text_proto_log: Some(true),
            auto_reload_config: None,
            claude_default_placement: None,
            theme: None,
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            terminal_font_family: None,
            terminal_font_size: None,
            terminal_font_ligatures: None,
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
            item_modes: BTreeMap::new(),
        };
        assert_eq!(
            left.merge(right),
            Settings {
                text_proto_log: Some(true),
                auto_reload_config: None,
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                terminal_font_family: None,
                terminal_font_size: None,
                terminal_font_ligatures: None,
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
                item_modes: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn merge_right_none_preserves_left() {
        let left = Settings {
            text_proto_log: Some(true),
            auto_reload_config: None,
            claude_default_placement: None,
            theme: None,
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            terminal_font_family: None,
            terminal_font_size: None,
            terminal_font_ligatures: None,
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
            item_modes: BTreeMap::new(),
        };
        let right = Settings::default();
        assert_eq!(
            left.merge(right),
            Settings {
                text_proto_log: Some(true),
                auto_reload_config: None,
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                terminal_font_family: None,
                terminal_font_size: None,
                terminal_font_ligatures: None,
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
                item_modes: BTreeMap::new(),
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
                auto_reload_config: None,
                claude_default_placement: Some(ClaudePlacement::Pane),
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                terminal_font_family: None,
                terminal_font_size: None,
                terminal_font_ligatures: None,
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
                item_modes: BTreeMap::new(),
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
                auto_reload_config: None,
                claude_default_placement: Some(ClaudePlacement::DockLeft),
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                terminal_font_family: None,
                terminal_font_size: None,
                terminal_font_ligatures: None,
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
                item_modes: BTreeMap::new(),
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
                auto_reload_config: None,
                claude_default_placement: Some(ClaudePlacement::DockRight),
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                terminal_font_family: None,
                terminal_font_size: None,
                terminal_font_ligatures: None,
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
                item_modes: BTreeMap::new(),
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
            auto_reload_config: None,
            claude_default_placement: Some(ClaudePlacement::DockRight),
            theme: None,
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            terminal_font_family: None,
            terminal_font_size: None,
            terminal_font_ligatures: None,
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
            item_modes: BTreeMap::new(),
        };
        let right = Settings::default();
        assert_eq!(
            left.clone().merge(right),
            Settings {
                text_proto_log: None,
                auto_reload_config: None,
                claude_default_placement: Some(ClaudePlacement::DockRight),
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                terminal_font_family: None,
                terminal_font_size: None,
                terminal_font_ligatures: None,
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
                item_modes: BTreeMap::new(),
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
                auto_reload_config: None,
                claude_default_placement: None,
                theme: Some("default_dark".into()),
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                terminal_font_family: None,
                terminal_font_size: None,
                terminal_font_ligatures: None,
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
                item_modes: BTreeMap::new(),
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
                auto_reload_config: None,
                claude_default_placement: None,
                theme: Some("default_dark".into()),
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                terminal_font_family: None,
                terminal_font_size: None,
                terminal_font_ligatures: None,
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
                item_modes: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn merge_right_overrides_theme() {
        let left = Settings {
            text_proto_log: None,
            auto_reload_config: None,
            claude_default_placement: None,
            theme: Some("a".into()),
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            terminal_font_family: None,
            terminal_font_size: None,
            terminal_font_ligatures: None,
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
            item_modes: BTreeMap::new(),
        };
        let right = Settings {
            text_proto_log: None,
            auto_reload_config: None,
            claude_default_placement: None,
            theme: Some("b".into()),
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            terminal_font_family: None,
            terminal_font_size: None,
            terminal_font_ligatures: None,
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
            item_modes: BTreeMap::new(),
        };
        assert_eq!(left.merge(right).theme, Some("b".into()));
    }

    #[test]
    fn merge_right_overrides_claude_placement() {
        let left = Settings {
            text_proto_log: None,
            auto_reload_config: None,
            claude_default_placement: Some(ClaudePlacement::Pane),
            theme: None,
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            terminal_font_family: None,
            terminal_font_size: None,
            terminal_font_ligatures: None,
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
            item_modes: BTreeMap::new(),
        };
        let right = Settings {
            text_proto_log: None,
            auto_reload_config: None,
            claude_default_placement: Some(ClaudePlacement::DockLeft),
            theme: None,
            mouse_capture: None,
            mode_badges: BTreeMap::new(),
            editor_font_family: None,
            editor_font_size: None,
            terminal_font_family: None,
            terminal_font_size: None,
            terminal_font_ligatures: None,
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
            item_modes: BTreeMap::new(),
        };
        assert_eq!(
            left.merge(right),
            Settings {
                text_proto_log: None,
                auto_reload_config: None,
                claude_default_placement: Some(ClaudePlacement::DockLeft),
                theme: None,
                mouse_capture: None,
                mode_badges: BTreeMap::new(),
                editor_font_family: None,
                editor_font_size: None,
                terminal_font_family: None,
                terminal_font_size: None,
                terminal_font_ligatures: None,
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
                item_modes: BTreeMap::new(),
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
    fn from_config_extracts_item_modes_string_and_ident() {
        let config = parse_ok(
            r#"on init {
                ui.item_mode.review = "review";
                ui.item_mode.rebase = rebase;
            }"#,
        );
        assert_eq!(
            Settings::from_config(&config).item_modes,
            BTreeMap::from([
                ("review".to_string(), "review".to_string()),
                ("rebase".to_string(), "rebase".to_string()),
            ])
        );
    }

    #[test]
    fn from_config_ignores_wrong_type_item_mode() {
        let config = parse_ok("on init { ui.item_mode.review = true; }");
        assert!(Settings::from_config(&config).item_modes.is_empty());
    }

    #[test]
    fn merge_extends_item_modes_with_right_winning() {
        let left = Settings {
            item_modes: BTreeMap::from([
                ("review".to_string(), "review".to_string()),
                ("conflict".to_string(), "normal".to_string()),
            ]),
            ..Settings::default()
        };
        let right = Settings {
            item_modes: BTreeMap::from([("conflict".to_string(), "conflict".to_string())]),
            ..Settings::default()
        };
        assert_eq!(
            left.merge(right).item_modes,
            BTreeMap::from([
                ("review".to_string(), "review".to_string()),
                ("conflict".to_string(), "conflict".to_string()),
            ])
        );
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
            terminal_font_family: None,
            terminal_font_size: None,
            terminal_font_ligatures: None,
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

    #[test]
    fn catalog_covers_every_recognized_key() {
        let keys: Vec<&str> = setting_catalog().iter().map(|d| d.key).collect();
        assert_eq!(
            keys,
            [
                "text_proto_log",
                "auto_reload_config",
                "claude.default_placement",
                "theme",
                "ui.mode_badge.*",
                "ui.item_mode.*",
                "mouse.capture",
                "editor.font.family",
                "editor.font.size",
                "terminal.font.family",
                "terminal.font.size",
                "terminal.font.ligatures",
                "ui.font.family",
                "ui.font.size",
                "ui.pane.show_tab_bar",
                "ui.pane.show_breadcrumbs",
                "ui.editor.show_scrollbar_markers",
                "ui.editor.show_inline_blame",
                "ui.editor.show_indent_guides",
                "ui.editor.show_sticky_scroll",
                "ui.editor.line_numbers",
                "ui.editor.show_whitespace",
                "lsp.*.command",
                "lsp.*.args",
                "lsp.*.env.*",
            ],
        );
    }

    #[test]
    fn catalog_enum_keys_expose_allowed_values() {
        let find = |key| {
            setting_catalog()
                .iter()
                .find(|d| d.key == key)
                .unwrap_or_else(|| panic!("missing {key}"))
        };
        assert_eq!(
            find("ui.editor.line_numbers").allowed,
            ["absolute", "relative", "hybrid"]
        );
        assert_eq!(
            find("ui.editor.show_whitespace").allowed,
            ["none", "boundary", "selection", "all"]
        );
        assert_eq!(
            find("claude.default_placement").allowed,
            ["pane", "dock-left", "dock-right"]
        );
        assert_eq!(find("mouse.capture").allowed, ["auto", "always", "never"]);
        assert!(find("theme").allowed.is_empty());
        // Every entry carries a doc string for hover/completion.
        assert!(setting_catalog().iter().all(|d| !d.doc.is_empty()));
    }

    #[test]
    fn catalog_applies_wildcard_keys() {
        let config = parse_ok(
            "on init {\n  \
             ui.mode_badge.normal = \"NOR\";\n  \
             ui.item_mode.editor = \"insert\";\n  \
             lsp.rust.command = \"rust-analyzer\";\n  \
             lsp.rust.args = [\"--stdio\"];\n  \
             lsp.rust.env.RUST_LOG = \"info\";\n}\n",
        );
        let s = Settings::from_config(&config);
        assert_eq!(s.mode_badges.get("normal").map(String::as_str), Some("NOR"));
        assert_eq!(
            s.item_modes.get("editor").map(String::as_str),
            Some("insert")
        );
        let rust = s.language_servers.get("rust").expect("rust lsp present");
        assert_eq!(rust.command, "rust-analyzer");
        assert_eq!(rust.args, ["--stdio".to_string()]);
        assert_eq!(rust.env.get("RUST_LOG").map(String::as_str), Some("info"));
    }

    #[test]
    fn runtime_settable_keys_are_the_bool_enum_subset() {
        let runtime: Vec<&str> = setting_catalog()
            .iter()
            .filter(|d| d.apply_raw.is_some())
            .map(|d| d.key)
            .collect();
        assert_eq!(
            runtime,
            [
                "auto_reload_config",
                "editor.font.family",
                "editor.font.size",
                "terminal.font.family",
                "terminal.font.size",
                "terminal.font.ligatures",
                "ui.font.family",
                "ui.font.size",
                "ui.pane.show_tab_bar",
                "ui.pane.show_breadcrumbs",
                "ui.editor.show_scrollbar_markers",
                "ui.editor.show_inline_blame",
                "ui.editor.show_indent_guides",
                "ui.editor.show_sticky_scroll",
                "ui.editor.line_numbers",
                "ui.editor.show_whitespace",
            ],
        );
        // A recognized key with no runtime setter is rejected, not silently set.
        let mut s = Settings::default();
        assert!(matches!(
            s.apply_runtime("theme", "dark"),
            Err(SettingsApplyError::UnknownKey { .. })
        ));
    }

    #[test]
    fn from_config_extracts_terminal_font() {
        let config =
            parse_ok("on init { terminal.font.family = \"Fira Code\"; terminal.font.size = 13; }");
        let s = Settings::from_config(&config);
        assert_eq!(s.terminal_font_family.as_deref(), Some("Fira Code"));
        assert_eq!(s.terminal_font_size, Some(13.0));
    }

    #[test]
    fn apply_runtime_sets_terminal_font_and_rejects_bad_size() {
        let mut s = Settings::default();
        s.apply_runtime("terminal.font.family", "Fira Code")
            .expect("family is runtime-settable");
        s.apply_runtime("terminal.font.size", "13")
            .expect("numeric size parses");
        assert_eq!(s.terminal_font_family.as_deref(), Some("Fira Code"));
        assert_eq!(s.terminal_font_size, Some(13.0));
        assert!(matches!(
            s.apply_runtime("terminal.font.size", "huge"),
            Err(SettingsApplyError::InvalidValue { .. })
        ));
    }

    #[test]
    fn apply_runtime_sets_editor_and_ui_fonts() {
        let mut s = Settings::default();
        s.apply_runtime("editor.font.family", "Fira Code")
            .expect("editor family is runtime-settable");
        s.apply_runtime("editor.font.size", "15")
            .expect("editor size parses");
        s.apply_runtime("ui.font.family", "SF Pro")
            .expect("ui family is runtime-settable");
        s.apply_runtime("ui.font.size", "13")
            .expect("ui size parses");
        assert_eq!(s.editor_font_family.as_deref(), Some("Fira Code"));
        assert_eq!(s.editor_font_size, Some(15.0));
        assert_eq!(s.ui_font_family.as_deref(), Some("SF Pro"));
        assert_eq!(s.ui_font_size, Some(13.0));
        assert!(matches!(
            s.apply_runtime("editor.font.size", "big"),
            Err(SettingsApplyError::InvalidValue { .. })
        ));
    }

    #[test]
    fn terminal_font_ligatures_setting() {
        let config = parse_ok("on init { terminal.font.ligatures = false; }");
        assert_eq!(
            Settings::from_config(&config).terminal_font_ligatures,
            Some(false)
        );

        let mut s = Settings::default();
        s.apply_runtime("terminal.font.ligatures", "true")
            .expect("bool is runtime-settable");
        assert_eq!(s.terminal_font_ligatures, Some(true));
        assert!(matches!(
            s.apply_runtime("terminal.font.ligatures", "maybe"),
            Err(SettingsApplyError::InvalidValue { .. })
        ));
    }
}
